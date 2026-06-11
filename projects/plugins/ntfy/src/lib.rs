//! ntfy.sh-compatible push notification client + orca-managed endpoint
//! registry. The plugin owns:
//!   - the [`Client`] + [`Message`] HTTP primitives,
//!   - a [`backend::NtfyBackend`] that implements `notifications::Backend`,
//!   - `ntfy.{add,list,delete,send}` `#[orca_tool]` CRUD surface,
//!   - a [`bootstrap`] entry point that loads `db::ntfy` rows and registers
//!     each enabled endpoint with the `notifications` dispatcher.
//!
//! `notifications` knows nothing about ntfy. Adding email/Slack/etc. follows
//! the same shape: own crate, own table, own backend impl, own bootstrap.
//!
//! Composes with `utils::http` for transport so HTTP bug fixes propagate.

pub mod backend;
pub mod tools;

use std::sync::Arc;

use crate::backend::NtfyBackend;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use utils::http::{Client as HttpClient, HttpError};

/// Stable connection config. Cheap to clone (`base` + `topic` + optional
/// bearer token).
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub topic: String,
    pub token: Option<String>,
}

impl Config {
    pub fn new(base_url: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            topic: topic.into(),
            token: None,
        }
    }
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

#[derive(Debug, Error)]
pub enum NtfyError {
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Per-message attributes. Optional; only `message` is required.
#[derive(Debug, Clone, Default)]
pub struct Message<'a> {
    pub message: &'a str,
    pub title: Option<&'a str>,
    pub priority: Option<Priority>,
    pub tags: Vec<&'a str>,
    pub click: Option<&'a str>,
    /// Override the configured topic for this single call.
    pub topic_override: Option<&'a str>,
    /// Render body as markdown on supporting clients (web UI, iOS/Android
    /// app v2+). Sends `X-Markdown: yes` and switches the body content type
    /// to `text/markdown`. Plain-text clients fall back to raw text.
    pub markdown: bool,
}

/// ntfy priority levels. Wire format is the lowercase name.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Min,
    Low,
    Default,
    High,
    Urgent,
}

impl Priority {
    fn as_header(self) -> &'static str {
        match self {
            Priority::Min => "min",
            Priority::Low => "low",
            Priority::Default => "default",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub status: u16,
    pub ok: bool,
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

    /// Send a notification.
    pub async fn send(&self, msg: Message<'_>) -> Result<SendResult, NtfyError> {
        if msg.message.is_empty() {
            return Err(NtfyError::Http(HttpError::InvalidUrl(
                "missing message".into(),
            )));
        }
        let topic = msg.topic_override.unwrap_or(&self.cfg.topic);
        let endpoint = format!(
            "{}/{}",
            self.cfg.base_url.trim_end_matches('/'),
            urlencoding::encode(topic)
        );

        let content_type = if msg.markdown {
            "text/markdown; charset=utf-8"
        } else {
            "text/plain; charset=utf-8"
        };
        let mut req = self
            .http
            .post(&endpoint)
            .bytes(msg.message.as_bytes().to_vec(), content_type);

        if msg.markdown {
            req = req.header("X-Markdown", "yes");
        }
        if let Some(title) = msg.title {
            req = req.header("X-Title", title);
        }
        if let Some(p) = msg.priority {
            req = req.header("X-Priority", p.as_header());
        }
        if !msg.tags.is_empty() {
            req = req.header("X-Tags", msg.tags.join(","));
        }
        if let Some(click) = msg.click {
            req = req.header("X-Click", click);
        }
        if let Some(token) = &self.cfg.token {
            req = req.bearer(token);
        }

        let resp = req.send().await?;
        Ok(SendResult {
            status: resp.status,
            ok: (200..300).contains(&resp.status),
        })
    }

    /// Convenience for health-watch loops: low-priority "ok" ping.
    pub async fn heartbeat(&self, topic_override: Option<&str>) -> Result<SendResult, NtfyError> {
        self.send(Message {
            message: "ok",
            priority: Some(Priority::Low),
            topic_override,
            ..Default::default()
        })
        .await
    }
}

// ── notifications wiring ───────────────────────────────────────────────────

/// Build an [`NtfyBackend`] from a db row and register it with the global
/// `notifications` dispatcher. Used both at daemon startup (via [`bootstrap`])
/// and live by `ntfy.add` so freshly-added endpoints work without a restart.
pub fn register_endpoint(row: &db::ntfy::EndpointRow) {
    let mut cfg = Config::new(row.base_url.clone(), row.topic.clone());
    if let Some(t) = &row.token {
        cfg = cfg.with_token(t.clone());
    }
    let backend = NtfyBackend::new(row.name.clone(), Client::new(cfg));
    notifications::register_backend(Arc::new(backend));
}

/// Daemon startup hook — load every enabled ntfy endpoint and register it as
/// a notifications backend. Non-fatal on db read errors so notifications
/// outages don't gate daemon boot.
pub fn bootstrap() {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("ntfy bootstrap: db open failed: {e}");
            return;
        }
    };
    let rows = match db::ntfy::list(&conn) {
        Ok(rs) => rs,
        Err(e) => {
            tracing::warn!("ntfy bootstrap: list failed: {e}");
            return;
        }
    };
    for row in rows.into_iter().filter(|r| r.enabled) {
        register_endpoint(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn send_posts_to_topic_with_headers() {
        // Verifies headers we set without locking to the join format of
        // X-Tags (wiremock 0.6's `header` matcher treats comma-separated
        // values as multi-valued, which doesn't match ntfy's wire format).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/homelab"))
            .and(header("x-title", "alert"))
            .and(header("x-priority", "high"))
            .and(header("authorization", "Bearer abc"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let c = Client::new(Config::new(server.uri(), "homelab").with_token("abc"));
        let r = c
            .send(Message {
                message: "disk full",
                title: Some("alert"),
                priority: Some(Priority::High),
                tags: vec!["warn", "urgent"],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(r.ok);
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn heartbeat_sends_low_priority() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/topic"))
            .and(header("X-Priority", "low"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let c = Client::new(Config::new(server.uri(), "topic"));
        let r = c.heartbeat(None).await.unwrap();
        assert!(r.ok);
    }

    #[tokio::test]
    async fn topic_override_takes_precedence() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/other"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let c = Client::new(Config::new(server.uri(), "default-topic"));
        c.send(Message {
            message: "x",
            topic_override: Some("other"),
            ..Default::default()
        })
        .await
        .unwrap();
    }
}
