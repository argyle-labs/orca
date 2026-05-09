//! ntfy.sh-compatible push notification client. Replaces the former `ntfy`
//! plugin: same surface (`send`, `heartbeat`), no plugin scaffolding.
//!
//! Composes with [`orca_http`] for transport so HTTP bug fixes propagate.

use orca_http::{Client as HttpClient, HttpError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

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

        let mut req = self
            .http
            .post(&endpoint)
            .bytes(msg.message.as_bytes().to_vec(), "text/plain; charset=utf-8");

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
        let c = Client::new(
            Config::new(server.uri(), "homelab").with_token("abc"),
        );
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
