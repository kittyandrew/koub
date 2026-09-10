use crate::{bauth, context::Context, db, db::Connection, notifications, prom};
use rocket::{State, http::Status, serde::json::Value, serde::json::json, tokio};

#[get("/api/v1/health")]
pub async fn api_health(_rl: bauth::RateLimitGuard) -> Value {
    // @TODO: proper healthcheck.
    json!({"status": 200})
}

#[get("/api/v1/up")]
pub async fn api_up(bauth: bauth::BAuth, mut conn: Connection<'_>, context: &State<Context>) -> Status {
    let uid_str = bauth.uid.to_string();
    let uptime_snapshot = {
        let mut guard = context.users.write().await;
        let Some(item) = guard.get_mut(&bauth.uid) else {
            // User was deleted between BAuth validation and here (race with delete_user)
            return Status::Unauthorized;
        };
        // Update last-seen metric
        let now_ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        prom::LAST_SEEN_TIMESTAMP.with_label_values(&[&uid_str]).set(now_ts);

        let in_maint = item.user.is_in_maintenance_window(notifications::utc_minute_of_day(now_ts as u64));

        match item.uptime.touch() {
            db::TouchResult::Connected if !in_maint => {
                // Clone with Uninitialized status so dispatch uses "device connected" title
                let mut notification_state = item.clone();
                notification_state.uptime.status = db::UpStatus::Uninitialized;
                tokio::spawn(notifications::dispatch_notifications(notification_state, context.inner().clone(), None));
            }
            db::TouchResult::Restored(duration) if !in_maint => {
                tokio::spawn(notifications::dispatch_notifications(item.clone(), context.inner().clone(), Some(duration)));
            }
            _ => {} // NoChange, or suppressed by maintenance window
        }
        // Update uptime state metric
        prom::UPTIME_STATE.with_label_values(&[&uid_str]).set(i64::from(&item.uptime.status));
        item.uptime.clone()
    };
    // Persist uptime state to DB (outside the write lock to avoid blocking)
    if let Err(err) = db::update_uptime_state(&mut conn.0, &uptime_snapshot).await {
        warn!("Failed to persist uptime state: {err:?}");
    }
    Status::Ok
}
