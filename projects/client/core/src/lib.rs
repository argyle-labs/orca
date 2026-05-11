//! Target-agnostic orca client core.
//!
//! Defines the [`Transport`] trait that target-specific crates (wasm, native,
//! swift, kotlin) implement, and the typed client surface that platform UIs
//! consume. All client-side business logic — request shaping, parsing,
//! retries, caching, polling — belongs here. Per-target crates contribute
//! only the bytes-in/bytes-out plumbing.

use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("status {0}: {1}")]
    Status(u16, String),
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[async_trait(?Send)]
pub trait Transport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, ClientError>;
}

pub struct OrcaClient<T: Transport> {
    base_url: String,
    transport: T,
}

impl<T: Transport> OrcaClient<T> {
    pub fn new(base_url: impl Into<String>, transport: T) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            transport,
        }
    }

    pub async fn health(&self) -> Result<Health, ClientError> {
        let req = HttpRequest {
            method: "GET",
            url: format!("{}/api/health", self.base_url),
        };
        let resp = self.transport.send(req).await?;
        if resp.status != 200 {
            let body = String::from_utf8_lossy(&resp.body).into_owned();
            return Err(ClientError::Status(resp.status, body));
        }
        serde_json::from_slice(&resp.body).map_err(|e| ClientError::Decode(e.to_string()))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub ok: bool,
}
