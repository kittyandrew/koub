use crate::db::NtfyUser;
use chbs::{config::BasicConfig, prelude::*, probability::Probability};
use lazy_static::lazy_static;
use rand::{Rng, distributions::Alphanumeric};
use reqwest::header;
use rocket::serde::{Deserialize, Serialize, json::Value};
use std::{env::var, error::Error, fmt};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

lazy_static! {
    pub static ref NTFY_BASE_URL: String = var("NTFY_BASE_URL").unwrap();
    pub static ref NTFY_USER_TIER: String = var("NTFY_USER_TIER").unwrap();
}

#[derive(Debug)]
struct NtfyCustomError {
    data: String,
}

impl NtfyCustomError {
    fn new(data: Value) -> NtfyCustomError {
        NtfyCustomError { data: data.to_string() }
    }
}

impl fmt::Display for NtfyCustomError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.data)
    }
}

impl Error for NtfyCustomError {}

#[derive(Debug, Clone)]
pub struct NtfyClient {
    base_url: String,
    base_tier: String,
    client: reqwest::Client,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(crate = "rocket::serde")]
pub struct NtfyNotification {
    pub topic: String,
    pub title: String,
    pub message: String,
    pub status: String,
    pub priority: String,
}

impl NtfyClient {
    // @TODO: Make optional result and turn off if creds not provided?
    pub fn new() -> NtfyClient {
        let auth_value = format!("Bearer {}", var("NTFY_ADMIN_TOKEN").unwrap());
        // Creating authorization header with the admin token.
        let mut auth_header = header::HeaderValue::from_str(&auth_value).unwrap();
        auth_header.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth_header);

        let client = reqwest::Client::builder().user_agent("OpenUptimeBot/v0").default_headers(headers).build().expect("RIP");

        NtfyClient { base_url: NTFY_BASE_URL.clone(), base_tier: NTFY_USER_TIER.clone(), client }
    }

    fn generate_new_user(&self, enabled: bool) -> NtfyUser {
        // @NOTE: Building custom passphrase gen configuration. I have no clue
        //  how secure this actually is and I did not evaluate this package for
        //  security. This basic passphrase config feels good enough for current
        //  use-case (read-only private notifications channel) combined with
        //  random topic and user name. The only change I would like to add
        //  is another word styler that randomly turns them into l33t spelling.
        let passgen = BasicConfig {
            words: 3,
            separator: "-".into(),
            capitalize_first: Probability::Never,
            capitalize_words: Probability::Never,
            ..BasicConfig::default()
        }
        .to_scheme();

        let mut rng = rand::thread_rng();
        // Random unique identifiers for user and topic.
        let topic_suffix: String = (&mut rng).sample_iter(&Alphanumeric).take(8).map(char::from).collect();
        let user_suffix: String = (&mut rng).sample_iter(&Alphanumeric).take(8).map(char::from).collect();
        NtfyUser {
            id: Uuid::new_v4(),
            enabled,
            topic: format!("topic_{topic_suffix}"),
            topic_permission: "ro".to_string(),
            username: format!("user_{user_suffix}"),
            password: passgen.generate(),
            tier: self.base_tier.clone(),
        }
    }

    pub async fn create_new_user(&self, enabled: bool) -> Result<NtfyUser> {
        let user = self.generate_new_user(enabled);

        let new_user_response = self.client.put(format!("{base}/v1/users", base = self.base_url)).json(&user).send().await?;

        if new_user_response.status() != 200 {
            return Err(Box::new(NtfyCustomError::new(new_user_response.json::<Value>().await?)));
        }

        let access_response = self.client.post(format!("{base}/v1/users/access", base = self.base_url)).json(&user).send().await?;

        if access_response.status() != 200 {
            return Err(Box::new(NtfyCustomError::new(access_response.json::<Value>().await?)));
        }

        Ok(user)
    }

    pub async fn delete_user(&self, username: &str) -> Result<()> {
        let response = self
            .client
            .delete(format!("{base}/v1/users", base = self.base_url))
            .json(&rocket::serde::json::json!({"username": username}))
            .send()
            .await?;

        if response.status() != 200 {
            return Err(Box::new(NtfyCustomError::new(response.json::<Value>().await?)));
        }

        Ok(())
    }

    pub async fn send_notification(&self, data: NtfyNotification) -> Result<()> {
        // Note we are still using admin authentication header here to write.
        self.client
            .post(format!("{base}/{topic}", base = self.base_url, topic = data.topic))
            .body(data.message)
            .header("X-Tags", data.status)
            .header("X-Title", data.title)
            .header("X-Priority", data.priority)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }
}
