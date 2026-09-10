use serde_json::Value;

pub struct Client {
    http: reqwest::blocking::Client,
    server: String,
    token: Option<String>,
}

impl Client {
    pub fn new(server: String, token: Option<String>) -> Self {
        Self { http: reqwest::blocking::Client::new(), server, token }
    }

    fn parse_response(resp: reqwest::blocking::Response) -> Result<Value, String> {
        let status = resp.status();

        // Handle common HTTP errors with user-friendly messages
        if status.as_u16() == 401 {
            return Err("Unauthorized: Invalid or missing token".to_string());
        }
        if status.as_u16() == 403 {
            return Err("Forbidden: You don't have permission to access this resource".to_string());
        }
        if status.as_u16() == 404 {
            return Err("Not found: The requested endpoint doesn't exist (is the server up-to-date?)".to_string());
        }
        if status.as_u16() == 429 {
            return Err("Rate limited: Too many requests, please wait".to_string());
        }
        if status.as_u16() >= 500 {
            return Err(format!("Server error (HTTP {})", status));
        }

        let body = resp.text().map_err(|e| format!("Failed to read response: {}", e))?;

        if body.is_empty() {
            return Err(format!("Empty response from server (HTTP {})", status));
        }

        serde_json::from_str(&body).map_err(|e| format!("Failed to parse JSON (HTTP {}): {}", status, e))
    }

    fn auth_header(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("token {}", t))
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.server, path);
        let mut req = self.http.get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().map_err(|e| format!("Request failed: {}", e))?;
        Self::parse_response(resp)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{}{}", self.server, path);
        let mut req = self.http.post(&url).json(body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().map_err(|e| format!("Request failed: {}", e))?;
        Self::parse_response(resp)
    }

    pub fn post_empty(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.server, path);
        let mut req = self.http.post(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().map_err(|e| format!("Request failed: {}", e))?;
        Self::parse_response(resp)
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value, String> {
        let url = format!("{}{}", self.server, path);
        let mut req = self.http.patch(&url).json(body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().map_err(|e| format!("Request failed: {}", e))?;
        Self::parse_response(resp)
    }

    pub fn delete(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.server, path);
        let mut req = self.http.delete(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().map_err(|e| format!("Request failed: {}", e))?;
        Self::parse_response(resp)
    }
}
