use crate::{context, db, prom};
use governor::{Quota, RateLimiter, clock::QuantaClock, state::InMemoryState};
use rocket::request::{FromRequest, Outcome};
use rocket::{Data, Request, State, fairing::Fairing, fairing::Info, fairing::Kind, http::Status};
use std::{net::IpAddr, num::NonZero, sync::Arc};

type IpLimiter = Arc<RateLimiter<IpAddr, dashmap::DashMap<IpAddr, InMemoryState>, QuantaClock>>;

pub fn get_rate_limiter() -> IpLimiter {
    // @NOTE: IP-based rate limiter covering ALL endpoints (applied as a fairing).
    //  5 req/sec per IP is generous enough for multi-device NAT setups while tight
    //  enough to slow brute-force attempts before fail2ban kicks in.
    Arc::new(RateLimiter::keyed(Quota::per_second(NonZero::new(5u32).unwrap())))
}

/// Rocket fairing that enforces per-IP rate limiting on all requests.
/// Fires before any endpoint logic, covering both authenticated and
/// unauthenticated endpoints (e.g., POST /api/v1/users invite brute-force).
pub struct IpRateLimitFairing;

#[rocket::async_trait]
impl Fairing for IpRateLimitFairing {
    fn info(&self) -> Info {
        Info { name: "IP Rate Limiter", kind: Kind::Request }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        match request.client_ip() {
            Some(ip) => {
                let limiter = request.guard::<&State<IpLimiter>>().await.unwrap();
                if limiter.check_key(&ip).is_err() {
                    warn!("[AUTH] ip={ip} result=rate_limited prefix=none");
                    prom::AUTH_FAILURES.with_label_values(&["rate_limited"]).inc();
                    // @NOTE: We set a flag in the request-local cache so BAuth/AdminAuth
                    //  guards can detect the rate limit and return 429. The fairing itself
                    //  can't short-circuit the request in Rocket 0.5.
                    request.local_cache(|| RateLimited(true));
                }
            }
            None => {
                warn!("[AUTH] ip=unknown result=no_client_ip — check reverse proxy configuration");
            }
        }
    }
}

#[derive(Copy, Clone)]
struct RateLimited(bool);

/// Extract a safe token prefix for logging. Never logs the full token.
fn token_prefix(raw: &str) -> &'static str {
    let token = raw.strip_prefix("token ").unwrap_or(raw);
    if token.starts_with("tk_") {
        // @NOTE: We return a static str to avoid lifetime issues. The prefix is only
        //  used for log categorization, not identification.
        if token.len() >= 9 { "tk_..." } else { "tk_short" }
    } else {
        "malformed"
    }
}

fn log_auth_failure(request: &Request<'_>, reason: &str, raw_header: Option<&str>) {
    let ip = request.client_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".to_string());
    let prefix = raw_header.map(token_prefix).unwrap_or("none");
    warn!("[AUTH] ip={ip} result={reason} prefix={prefix}");
    prom::AUTH_FAILURES.with_label_values(&[reason]).inc();
}

/// Lightweight guard that enforces the IP rate limit flag set by IpRateLimitFairing.
/// Add this to any endpoint that doesn't use BAuth/AdminAuth (which already check the flag).
pub struct RateLimitGuard;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RateLimitGuard {
    type Error = BAuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        if req.local_cache(|| RateLimited(false)).0 {
            return Outcome::Error((Status::TooManyRequests, BAuthError::RateLimited));
        }
        Outcome::Success(RateLimitGuard)
    }
}

#[derive(Debug)]
pub struct BAuth {
    pub uid: db::ID,
}

#[derive(Debug)]
pub struct AdminAuth {
    pub uid: db::ID,
}

#[derive(Debug)]
pub enum BAuthError {
    Missing,
    Invalid,
    RateLimited,
    NotAdmin,
}

/// Shared token resolution: rate limit check, header extraction, token lookup, failure logging.
/// Returns the authenticated user ID or an error outcome.
async fn resolve_token(req: &Request<'_>) -> Result<db::ID, (Status, BAuthError)> {
    // Check if the IP rate limiter already rejected this request
    if req.local_cache(|| RateLimited(false)).0 {
        return Err((Status::TooManyRequests, BAuthError::RateLimited));
    }

    match req.headers().get_one("Authorization") {
        Some(raw) => {
            let tokens = &req.guard::<&State<context::Context>>().await.unwrap().tokens;
            match tokens.read().await.get(raw.strip_prefix("token ").unwrap_or(raw)) {
                Some(uid) => Ok(*uid),
                None => {
                    log_auth_failure(req, "invalid_token", Some(raw));
                    Err((Status::Unauthorized, BAuthError::Invalid))
                }
            }
        }
        None => {
            log_auth_failure(req, "missing_header", None);
            Err((Status::Unauthorized, BAuthError::Missing))
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for BAuth {
    type Error = BAuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        match resolve_token(req).await {
            Ok(uid) => Outcome::Success(BAuth { uid }),
            Err(e) => Outcome::Error(e),
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AdminAuth {
    type Error = BAuthError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let uid = match resolve_token(req).await {
            Ok(uid) => uid,
            Err(e) => return Outcome::Error(e),
        };

        // Check if user is admin
        let context = req.guard::<&State<context::Context>>().await.unwrap();
        let users = context.users.read().await;
        match users.get(&uid) {
            Some(state) if state.user.user_type == db::UserType::Admin => Outcome::Success(AdminAuth { uid }),
            Some(_) => Outcome::Error((Status::Forbidden, BAuthError::NotAdmin)),
            None => Outcome::Error((Status::Unauthorized, BAuthError::Invalid)),
        }
    }
}
