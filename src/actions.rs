use crate::{context::Context, db, db::Invite, db::User, db::UserState};
use diesel_async::AsyncPgConnection;
use rocket::serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(crate = "rocket::serde")]
pub struct NewUser {
    pub invite: Option<String>,
    pub user_type: db::UserType,
    pub invites_limit: i64,
    pub up_delay: Option<u16>,
    pub ntfy_enabled: bool,
    /// Language code for notifications (e.g., "uk", "en")
    pub language_code: String,
}

/// Validate language code: must be 2-3 lowercase ASCII letters (ISO 639 format).
/// Unsupported codes are accepted but will fall back to English for notifications.
pub fn validate_language_code(lang: &str) -> Result<(), String> {
    if lang.len() < 2 || lang.len() > 3 || !lang.chars().all(|c| c.is_ascii_lowercase()) {
        return Err("Invalid language code: must be 2-3 lowercase letters (e.g., \"uk\", \"en\")".to_string());
    }
    if !crate::notifications::SUPPORTED_LOCALES.contains(&lang) {
        warn!("Language code '{lang}' is not a supported locale, notifications will fall back to English");
    }
    Ok(())
}

pub async fn create_user(opts: &NewUser, conn: &mut AsyncPgConnection, context: &Context) -> Result<UserState, String> {
    let mut invite_id: Option<db::ID> = None;
    let mut invite_token_key: Option<String> = None;

    // @NOTE: Hold init_lock for the no-invite (first-user) path to prevent TOCTOU race
    //  where concurrent requests both see an empty user map and create multiple admins.
    //  The lock is only contended during the one-time bootstrap; invite-based creation
    //  skips it entirely.
    let _init_guard = if opts.invite.is_none() { Some(context.init_lock.lock().await) } else { None };

    if let Some(new_invite) = &opts.invite {
        let tokens = context.invite_tokens.read().await;
        match tokens.get(new_invite) {
            Some(id) => {
                invite_id = Some(*id);
                invite_token_key = Some(new_invite.clone());
            }
            None => return Err("Provided invite token does not exist!".to_string()),
        }
    } else {
        // No invite provided - only allow Admin creation if no users exist (first init)
        let users = context.users.read().await;
        if !users.is_empty() {
            return Err("Invite token is required to create new users".to_string());
        }
        if opts.user_type != db::UserType::Admin {
            return Err("First user must be an Admin".to_string());
        }
    }

    // Invited users are always Normal with zero invites — only first-init (no invite) can be Admin
    let (user_type, invites_limit) =
        if invite_id.is_some() { (db::UserType::Normal, 0) } else { (opts.user_type, opts.invites_limit) };

    // Validate up_delay if provided
    if let Some(up_delay) = opts.up_delay
        && (!(10..=32767).contains(&up_delay))
    {
        return Err("up_delay must be between 10 and 32767 seconds".to_string());
    }

    validate_language_code(&opts.language_code)?;

    let ntfy = match context.ntfy.create_new_user(opts.ntfy_enabled).await {
        Ok(new_ntfy_user) => new_ntfy_user,
        Err(err) => return Err(format!("{err:?}")),
    };
    let new_user = User::new(user_type, invites_limit, opts.up_delay, opts.language_code.clone(), &ntfy);
    let new_state = db::UserState { uptime: db::UptimeState::new(new_user.id), user: new_user, ntfy };

    if let Err(err) = db::create_new_state(conn, &new_state, invite_id.as_ref()).await {
        // Clean up the ntfy user we already created on the external server
        if let Err(cleanup_err) = context.ntfy.delete_user(&new_state.ntfy.username).await {
            warn!("Failed to clean up ntfy user '{}' after DB error: {cleanup_err:?}", new_state.ntfy.username);
        }
        return Err(format!("{err:?}"));
    };

    // Remove consumed invite from in-memory map
    if let Some(key) = invite_token_key {
        context.invite_tokens.write().await.remove(&key);
    }

    context.add_state(new_state.clone()).await;
    Ok(new_state)
}

#[derive(Debug, Deserialize)]
#[serde(crate = "rocket::serde")]
pub struct NewInvite {
    pub owner_id: db::ID,
}

pub async fn create_invite(opts: &NewInvite, conn: &mut AsyncPgConnection, context: &Context) -> Result<Invite, String> {
    {
        let users = context.users.read().await;
        match users.get(&opts.owner_id) {
            Some(state) if state.user.invites_used >= state.user.invites_limit => {
                return Err("Invites used matching invites limit, early exiting!".to_string());
            }
            None => return Err("User was not found for given 'owner_id'!".to_string()),
            _ => {}
        };
    }

    let new_invite = db::Invite::new(opts.owner_id);
    match db::create_new_invite(conn, &new_invite).await {
        Ok(_) => {
            context.add_invite(new_invite.clone()).await;
            Ok(new_invite)
        }
        Err(err) => Err(format!("DB Err: {err:?}")),
    }
}
