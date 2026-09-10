mod models;
pub use models::*;

use crate::schema::{invites, ntfy_users, uptime_states, users};
use diesel::prelude::*;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, pooled_connection::bb8};
use rand::{Rng, distributions::Alphanumeric};
use rocket::{http::Status, request::FromRequest, request::Outcome, request::Request};
use uuid::Uuid;

type R<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub type Pool = bb8::Pool<AsyncPgConnection>;
pub struct Connection<'r>(pub bb8::PooledConnection<'r, AsyncPgConnection>);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Connection<'r> {
    type Error = bb8::RunError;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let pool = request.rocket().state::<Pool>().expect("database pool initialized");
        match pool.get().await {
            Ok(conn) => Outcome::Success(Connection(conn)),
            Err(error) => {
                error!("Failed to get database connection: {error}");
                Outcome::Error((Status::ServiceUnavailable, error))
            }
        }
    }
}

#[derive(Debug)]
enum CustomError {
    #[expect(dead_code)]
    DieselError(diesel::result::Error),
    CreationFailed,
}

impl From<diesel::result::Error> for CustomError {
    fn from(e: diesel::result::Error) -> Self {
        CustomError::DieselError(e)
    }
}

pub async fn create_new_state(
    conn: &mut AsyncPgConnection, user_state: &UserState, token_id: Option<&Uuid>,
) -> Result<(), String> {
    let result = conn
        .transaction::<_, CustomError, _>(async |tconn| {
            diesel::insert_into(ntfy_users::dsl::ntfy_users)
                .values(&user_state.ntfy)
                .returning(ntfy_users::dsl::id)
                .get_result::<ID>(tconn)
                .await?;
            diesel::insert_into(users::dsl::users)
                .values(&user_state.user)
                .returning(users::dsl::id)
                .get_result::<ID>(tconn)
                .await?;
            diesel::insert_into(uptime_states::dsl::uptime_states)
                .values(&user_state.uptime)
                .returning(uptime_states::dsl::id)
                .get_result::<ID>(tconn)
                .await?;

            // Consume the invite (after user insert, since user_id has FK to users)
            if let Some(invite_id) = token_id {
                let updated = diesel::update(invites::dsl::invites)
                    .filter(invites::dsl::id.eq(invite_id))
                    .filter(invites::dsl::is_used.eq(false))
                    .set((invites::dsl::is_used.eq(true), invites::dsl::user_id.eq(user_state.user.id)))
                    .execute(tconn)
                    .await?;
                if updated == 0 {
                    return Err(CustomError::CreationFailed);
                }
            }

            Ok(())
        })
        .await;

    result.map_err(|err| format!("{err:?}"))
}

pub async fn get_all_states(conn: &mut AsyncPgConnection) -> R<Vec<UserState>> {
    let user_items: Vec<(User, NtfyUser)> = users::dsl::users
        .inner_join(ntfy_users::dsl::ntfy_users)
        .select((User::as_select(), NtfyUser::as_select()))
        .load::<(User, NtfyUser)>(conn)
        .await?;

    let mut all_states = Vec::new();
    for (user, ntfy) in user_items {
        let uptime = uptime_states::dsl::uptime_states
            .filter(uptime_states::dsl::user_id.eq(user.id))
            .order(uptime_states::dsl::created_at.desc())
            .select(UptimeState::as_select())
            .first::<UptimeState>(conn)
            .await?;
        all_states.push(UserState { user, ntfy, uptime });
    }

    Ok(all_states)
}

pub async fn create_new_invite(conn: &mut AsyncPgConnection, invite: &Invite) -> Result<(), String> {
    let owner_id = invite.owner_id.ok_or_else(|| "Invite must have an owner".to_string())?;
    let result = conn
        .transaction::<_, CustomError, _>(async |tconn| {
            let (limit, used) = diesel::update(users::dsl::users)
                .filter(users::dsl::id.eq(owner_id))
                .set(users::dsl::invites_used.eq(users::dsl::invites_used + 1))
                .returning((users::dsl::invites_limit, users::dsl::invites_used))
                .get_result::<(i64, i64)>(tconn)
                .await?;

            // Abort operation if limit was passed.
            if used > limit {
                warn!("New invite failed: {used}/{limit} for uid {}!", owner_id);
                return Err(CustomError::CreationFailed);
            }

            diesel::insert_into(invites::dsl::invites).values(invite).returning(invites::dsl::id).get_result::<ID>(tconn).await?;

            Ok(())
        })
        .await;

    result.map_err(|err| format!("{err:?}"))
}

pub async fn get_invites_for_user(conn: &mut AsyncPgConnection, uid: ID) -> Result<Vec<Invite>, diesel::result::Error> {
    invites::dsl::invites.filter(invites::dsl::owner_id.eq(uid)).load::<Invite>(conn).await
}

pub async fn delete_invite(conn: &mut AsyncPgConnection, invite_id: ID, owner_id: ID) -> Result<usize, diesel::result::Error> {
    conn.transaction::<_, diesel::result::Error, _>(async |tconn| {
        let invite_used = match invites::dsl::invites
            .filter(invites::dsl::id.eq(invite_id))
            .filter(invites::dsl::owner_id.eq(owner_id))
            .select(invites::dsl::is_used)
            .first::<bool>(tconn)
            .await
        {
            Ok(used) => used,
            Err(diesel::result::Error::NotFound) => return Ok(0),
            Err(e) => return Err(e),
        };

        let deleted = diesel::delete(
            invites::dsl::invites.filter(invites::dsl::id.eq(invite_id)).filter(invites::dsl::owner_id.eq(owner_id)),
        )
        .execute(tconn)
        .await?;

        // Reclaim invite slot if the deleted invite was unused
        if deleted > 0 && !invite_used {
            diesel::update(users::dsl::users.filter(users::dsl::id.eq(owner_id)))
                .set(users::dsl::invites_used.eq(users::dsl::invites_used - 1))
                .execute(tconn)
                .await?;
        }

        Ok(deleted)
    })
    .await
}

pub async fn delete_user(conn: &mut AsyncPgConnection, user_id: ID) -> Result<usize, diesel::result::Error> {
    conn.transaction::<_, diesel::result::Error, _>(async |tconn| {
        // Get ntfy_id before deleting user
        let ntfy_id: ID = users::dsl::users.filter(users::dsl::id.eq(user_id)).select(users::dsl::ntfy_id).first(tconn).await?;

        // Delete user (cascades uptime_states and invites via ON DELETE CASCADE)
        let deleted = diesel::delete(users::dsl::users.filter(users::dsl::id.eq(user_id))).execute(tconn).await?;

        // Delete ntfy user (FK points from users -> ntfy, so no cascade)
        diesel::delete(ntfy_users::dsl::ntfy_users.filter(ntfy_users::dsl::id.eq(ntfy_id))).execute(tconn).await?;

        Ok(deleted)
    })
    .await
}

// User settings management

pub async fn regenerate_user_token(conn: &mut AsyncPgConnection, user_id: ID) -> Result<String, diesel::result::Error> {
    let rng = rand::thread_rng();
    let secret_part: String = rng.sample_iter(&Alphanumeric).take(16).map(char::from).collect();
    let new_token = format!("tk_{secret_part}");

    diesel::update(users::dsl::users.filter(users::dsl::id.eq(user_id)))
        .set(users::dsl::access_token.eq(&new_token))
        .execute(conn)
        .await?;

    Ok(new_token)
}

pub async fn update_ntfy_enabled(conn: &mut AsyncPgConnection, ntfy_id: ID, enabled: bool) -> Result<(), diesel::result::Error> {
    diesel::update(ntfy_users::dsl::ntfy_users.filter(ntfy_users::dsl::id.eq(ntfy_id)))
        .set(ntfy_users::dsl::enabled.eq(enabled))
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn update_user_language(
    conn: &mut AsyncPgConnection, user_id: ID, language_code: &str,
) -> Result<(), diesel::result::Error> {
    diesel::update(users::dsl::users.filter(users::dsl::id.eq(user_id)))
        .set(users::dsl::language_code.eq(language_code))
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn update_uptime_state(conn: &mut AsyncPgConnection, state: &UptimeState) -> Result<(), diesel::result::Error> {
    diesel::update(uptime_states::dsl::uptime_states.filter(uptime_states::dsl::id.eq(state.id)))
        .set((
            uptime_states::dsl::touched_at.eq(state.touched_at),
            uptime_states::dsl::status.eq(state.status),
            uptime_states::dsl::state_changed_at.eq(state.state_changed_at),
            uptime_states::dsl::pre_pause_status.eq(state.pre_pause_status),
        ))
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn update_user_settings(
    conn: &mut AsyncPgConnection, user_id: ID, up_delay: Option<i16>, maint_start: Option<Option<i16>>,
    maint_end: Option<Option<i16>>,
) -> Result<(), diesel::result::Error> {
    let maint = match (maint_start, maint_end) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    let target = users::dsl::users.filter(users::dsl::id.eq(user_id));
    match (up_delay, maint) {
        (Some(d), Some((s, e))) => {
            diesel::update(target)
                .set((
                    users::dsl::up_delay.eq(d),
                    users::dsl::maint_window_start_utc.eq(s),
                    users::dsl::maint_window_end_utc.eq(e),
                ))
                .execute(conn)
                .await?;
        }
        (Some(d), None) => {
            diesel::update(target).set(users::dsl::up_delay.eq(d)).execute(conn).await?;
        }
        (None, Some((s, e))) => {
            diesel::update(target)
                .set((users::dsl::maint_window_start_utc.eq(s), users::dsl::maint_window_end_utc.eq(e)))
                .execute(conn)
                .await?;
        }
        (None, None) => {}
    }
    Ok(())
}

pub async fn get_all_unused_invites(conn: &mut AsyncPgConnection) -> Result<Vec<Invite>, diesel::result::Error> {
    invites::dsl::invites.filter(invites::dsl::is_used.eq(false)).load::<Invite>(conn).await
}
