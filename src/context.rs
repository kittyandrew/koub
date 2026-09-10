use crate::{db::ID, db::Invite, db::UserState, ntfy::NtfyClient};
use rocket::tokio::sync::{Mutex, RwLock};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct Context {
    pub users: Arc<RwLock<HashMap<ID, UserState>>>,
    pub tokens: Arc<RwLock<HashMap<String, ID>>>,
    pub invite_tokens: Arc<RwLock<HashMap<String, ID>>>,
    /// Serializes first-user (admin init) creation to prevent TOCTOU race.
    pub init_lock: Arc<Mutex<()>>,
    pub ntfy: NtfyClient,
}

impl Context {
    pub fn init() -> Context {
        Context {
            users: Default::default(),
            tokens: Default::default(),
            invite_tokens: Default::default(),
            init_lock: Default::default(),
            ntfy: NtfyClient::new(),
        }
    }

    pub async fn add_state(&self, v: UserState) {
        self.tokens.write().await.insert(v.user.access_token.clone(), v.user.id);

        if let Some(old) = self.users.write().await.insert(v.user.id, v) {
            warn!("Creating new user state, but one already existed: {old:?}");
        }
    }

    pub async fn add_invite(&self, v: Invite) {
        self.invite_tokens.write().await.insert(v.token, v.id);

        if let Some(owner_id) = v.owner_id {
            let mut users = self.users.write().await;
            if let Some(state) = users.get_mut(&owner_id) {
                state.user.invites_used += 1;
            }
        }
    }

    /// Remove an invite from in-memory state. Returns true if the invite was
    /// found in the tokens map (meaning it was unused).
    pub async fn remove_invite(&self, invite_id: ID) -> bool {
        let mut invite_tokens = self.invite_tokens.write().await;
        let before = invite_tokens.len();
        invite_tokens.retain(|_, id| *id != invite_id);
        invite_tokens.len() < before
    }

    /// Remove a user from in-memory state
    pub async fn remove_user(&self, user_id: ID) {
        if let Some(state) = self.users.write().await.remove(&user_id) {
            self.tokens.write().await.remove(&state.user.access_token);
        }
    }

    /// Remove invite tokens by invite IDs (used when cascade-deleting a user's invites)
    pub async fn remove_invite_ids(&self, invite_ids: &[ID]) {
        if invite_ids.is_empty() {
            return;
        }
        self.invite_tokens.write().await.retain(|_, id| !invite_ids.contains(id));
    }
}
