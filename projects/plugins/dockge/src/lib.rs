//! Dockge stack-manager plugin. Client + tool surface.
//!
//! Tool surface (mirrors home-assistant's flat 4-tool shape per
//! [[feedback-one-tool-per-resource]]):
//! - `dockge.list` — registered endpoints (no args), or stacks on one
//!   endpoint (`endpoint=` arg).
//! - `dockge.detail` — logs for a single `(endpoint, stack)`.
//! - `dockge.update` — register/update an endpoint, or run a stack
//!   action (start / stop / restart) via the `action` arg.
//! - `dockge.delete` — remove a registered endpoint.
//!
//! Endpoint resolution: tools accept the endpoint *name* and load
//! `(base_url, token)` from `db::dockge` at call time. Per
//! [[project-colocated-api-clients]] + model B (any creds-holder may
//! execute), this row syncs to every paired peer so any of them can
//! call dockge.* against a registered endpoint.
// Upstream Dockge stack-list and log payloads are opaque per their API.
#![allow(clippy::disallowed_types)]

pub mod tools;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use utils::http::{Client as HttpClient, HttpError};

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub token: String,
}

impl Config {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum DockgeError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("missing 'stack' name")]
    MissingStack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub stack: String,
    pub status: u16,
}

#[derive(Clone)]
pub struct Client {
    cfg: Config,
    http: HttpClient,
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            http: HttpClient::new(),
        }
    }

    pub fn with_http(cfg: Config, http: HttpClient) -> Self {
        Self { cfg, http }
    }

    /// List all Dockge stacks with their running status.
    pub async fn list(&self) -> Result<Value, DockgeError> {
        self.get("/api/stacks").await
    }

    /// Start a stack.
    pub async fn start(&self, stack: &str) -> Result<ActionResult, DockgeError> {
        self.action(stack, "start").await
    }

    /// Stop a stack.
    pub async fn stop(&self, stack: &str) -> Result<ActionResult, DockgeError> {
        self.action(stack, "stop").await
    }

    /// Restart a stack (stop + start; some Dockge versions expose this directly).
    pub async fn restart(&self, stack: &str) -> Result<ActionResult, DockgeError> {
        self.action(stack, "restart").await
    }

    /// Recent logs for a stack.
    pub async fn logs(&self, stack: &str) -> Result<Value, DockgeError> {
        if stack.is_empty() {
            return Err(DockgeError::MissingStack);
        }
        self.get(&format!("/api/stacks/{}/logs", urlencoding::encode(stack)))
            .await
    }

    async fn action(&self, stack: &str, op: &'static str) -> Result<ActionResult, DockgeError> {
        if stack.is_empty() {
            return Err(DockgeError::MissingStack);
        }
        let path = format!("/api/stacks/{}/{}", urlencoding::encode(stack), op);
        let resp = self
            .http
            .post(self.url(&path))
            .bearer(&self.cfg.token)
            .send()
            .await?;
        Ok(ActionResult {
            stack: stack.to_string(),
            status: resp.status,
        })
    }

    async fn get(&self, path: &str) -> Result<Value, DockgeError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer(&self.cfg.token)
            .send()
            .await?;
        Ok(resp
            .json::<Value>()
            .unwrap_or_else(|_| serde_json::json!({"raw": resp.text()})))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_returns_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stacks"))
            .and(header("authorization", "Bearer t"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"stacks": []})),
            )
            .mount(&server)
            .await;
        let v = Client::new(Config::new(server.uri(), "t"))
            .list()
            .await
            .unwrap();
        assert!(v["stacks"].is_array());
    }

    #[tokio::test]
    async fn start_hits_action_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/stacks/web/start"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let r = Client::new(Config::new(server.uri(), "t"))
            .start("web")
            .await
            .unwrap();
        assert_eq!(r.stack, "web");
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn empty_stack_name_rejected_before_call() {
        let c = Client::new(Config::new("http://nope", "t"));
        let err = c.stop("").await.unwrap_err();
        assert!(matches!(err, DockgeError::MissingStack));
    }
}
