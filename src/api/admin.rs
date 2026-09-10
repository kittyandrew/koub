use crate::{actions, actions::NewInvite, bauth, context::Context, db, db::Connection, prom};
use rocket::{State, serde::json::Value, serde::json::json};

/// Create a new invite (admin only)
#[post("/api/v1/invites")]
pub async fn create_invite(admin: bauth::AdminAuth, mut conn: Connection<'_>, context: &State<Context>) -> Value {
    let opts = NewInvite { owner_id: admin.uid };
    match actions::create_invite(&opts, &mut conn.0, context).await {
        Ok(invite) => json!({"status": 200, "invite": invite}),
        Err(err) => json!({"status": 400, "error": err}),
    }
}

/// List all invites (admin only)
#[get("/api/v1/invites")]
pub async fn list_invites(admin: bauth::AdminAuth, mut conn: Connection<'_>) -> Value {
    match db::get_invites_for_user(&mut conn.0, admin.uid).await {
        Ok(invites) => json!({"status": 200, "invites": invites}),
        Err(err) => json!({"status": 500, "error": format!("{err:?}")}),
    }
}

/// Delete an invite (admin only)
#[delete("/api/v1/invites/<invite_id>")]
pub async fn delete_invite(
    admin: bauth::AdminAuth, invite_id: uuid::Uuid, mut conn: Connection<'_>, context: &State<Context>,
) -> Value {
    match db::delete_invite(&mut conn.0, invite_id, admin.uid).await {
        Ok(deleted) if deleted > 0 => {
            let was_unused = context.remove_invite(invite_id).await;
            if was_unused && let Some(state) = context.users.write().await.get_mut(&admin.uid) {
                state.user.invites_used -= 1;
            }
            json!({"status": 200, "message": "Invite deleted"})
        }
        Ok(_) => json!({"status": 404, "error": "Invite not found or not owned by you"}),
        Err(err) => json!({"status": 500, "error": format!("{err:?}")}),
    }
}

/// Delete a user (admin only)
#[delete("/api/v1/admin/users/<user_id>")]
pub async fn delete_user(
    admin: bauth::AdminAuth, user_id: uuid::Uuid, mut conn: Connection<'_>, context: &State<Context>,
) -> Value {
    if admin.uid == user_id {
        return json!({"status": 400, "error": "Cannot delete yourself"});
    }
    // Collect data needed for cleanup before deletion (DB will cascade-delete invites)
    let invite_ids: Vec<uuid::Uuid> = match db::get_invites_for_user(&mut conn.0, user_id).await {
        Ok(invites) => invites.iter().filter(|i| !i.is_used).map(|i| i.id).collect(),
        Err(err) => {
            warn!("Failed to load invites for user {user_id} during deletion: {err:?}");
            vec![]
        }
    };
    let ntfy_username = context.users.read().await.get(&user_id).map(|s| s.ntfy.username.clone());
    match db::delete_user(&mut conn.0, user_id).await {
        Ok(deleted) if deleted > 0 => {
            context.remove_user(user_id).await;
            context.remove_invite_ids(&invite_ids).await;
            // Clean up per-user metrics
            let uid_str = user_id.to_string();
            let _ = prom::UPTIME_STATE.remove_label_values(&[&uid_str]);
            let _ = prom::LAST_SEEN_TIMESTAMP.remove_label_values(&[&uid_str]);
            prom::ACTIVE_USERS.dec();
            // Clean up ntfy.sh server user
            if let Some(username) = ntfy_username
                && let Err(err) = context.ntfy.delete_user(&username).await
            {
                warn!("Failed to delete ntfy user '{username}': {err:?}");
            }
            json!({"status": 200, "message": "User deleted"})
        }
        Ok(_) => json!({"status": 404, "error": "User not found"}),
        Err(err) => json!({"status": 500, "error": format!("{err:?}")}),
    }
}

/// List all users (admin only).
/// @NOTE: Intentionally exposes full user data including access_token and ntfy
///  credentials — admins have explicit access to manage and impersonate any user.
#[get("/api/v1/admin/users")]
pub async fn admin_list_users(_admin: bauth::AdminAuth, context: &State<Context>) -> Value {
    let users: Vec<db::UserState> = context.users.read().await.values().cloned().collect();
    json!({"status": 200, "users": users})
}

/// Get any user by ID (admin only)
#[get("/api/v1/admin/users/<uid>")]
pub async fn admin_get_user(_admin: bauth::AdminAuth, uid: uuid::Uuid, context: &State<Context>) -> Value {
    match context.users.read().await.get(&uid) {
        Some(state) => json!({"status": 200, "user": state}),
        None => json!({"status": 404, "error": "User not found"}),
    }
}
