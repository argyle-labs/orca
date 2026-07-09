//! Concrete `DaemonClient` — the CLI/embedder → local-daemon HTTP transport.
//!
//! This is the ONE place the reqwest + rustls HTTP client for the CLI→daemon
//! round-trip lives. `dispatch` defines the [`dispatch::cli::DaemonClient`]
//! trait and routes `exec_local_daemon` / `post_daemon_raw` / `fetch_unit_ops`
//! through it, but links no HTTP stack itself — that is what lets a plugin
//! (which links `dispatch` only for the `register_op!` / `OrcaTool` surface)
//! shed reqwest/rustls entirely. See [[plugins-stay-thin]].
//!
//! The orca binary installs one of these at startup via
//! [`dispatch::cli::set_daemon_client`].

use anyhow::Result;
use dispatch::cli::{DaemonClient, local_daemon_url};
use std::future::Future;
use std::pin::Pin;

/// reqwest-backed daemon client. Auth mirrors the web UI: an operator session
/// cookie when present, else the owner-only loopback token minted by the daemon
/// at startup (fresh/headless nodes with no `orca auth login` yet).
pub struct HttpDaemonClient;

impl HttpDaemonClient {
    /// Install the reqwest daemon client as the process-global transport.
    pub fn install() {
        dispatch::cli::set_daemon_client(Box::new(HttpDaemonClient));
    }
}

/// Read the on-disk CLI session id written by `orca auth login`. Mode 0600 at
/// `$ORCA_HOME/session`. `None` if absent / unreadable / empty — the daemon then
/// rejects with 401 and the CLI surfaces "run `orca auth login` first".
fn read_session_id() -> Option<String> {
    let dir = contract::config::orca_home()?;
    let raw = std::fs::read_to_string(dir.join("session")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Read the process-local loopback token the daemon minted at startup
/// (`$ORCA_HOME/secrets/loopback.token`, mode 0600). The CLI runs as the daemon
/// owner, so on a host with no operator session yet it can still authenticate to
/// its LOCAL daemon with this owner-only secret. Only ever sent to loopback.
fn read_loopback_token() -> Option<String> {
    let dir = contract::config::orca_home()?;
    let raw = std::fs::read_to_string(dir.join("secrets").join("loopback.token")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Ensure the process-global ring crypto provider is installed. reqwest's
/// `rustls-no-provider` feature requires this before any client is built (TLS
/// support is detected at construct time, even for plain HTTP). Idempotent.
fn ensure_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
}

impl DaemonClient for HttpDaemonClient {
    #[allow(clippy::disallowed_types)]
    fn post_tool<'a>(
        &'a self,
        name: &'a str,
        args: serde_json::Value,
        correlation_id: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!("{}/api/v1/{}", local_daemon_url(), name);
            ensure_crypto_provider();
            let mut req = reqwest::Client::new().post(&url).json(&args);
            if let Some(sid) = read_session_id() {
                // Daemon middleware accepts cookie or bearer for the same session
                // row. Cookie form keeps us bit-for-bit identical to the UI.
                req = req.header("cookie", format!("orca_session={sid}"));
            } else if let Some(tok) = read_loopback_token() {
                req = req.header("authorization", format!("Bearer {tok}"));
            }
            if let Some(cid) = correlation_id {
                req = req.header("x-correlation-id", cid);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("local daemon returned {status} for {name}: {}", text.trim());
            }
            serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("decode {name} output: {e} (body: {text})"))
        })
    }

    #[allow(clippy::disallowed_types)]
    fn get_json<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!("{}{}", local_daemon_url(), path);
            ensure_crypto_provider();
            let mut req = reqwest::Client::new().get(&url);
            if let Some(sid) = read_session_id() {
                req = req.header("cookie", format!("orca_session={sid}"));
            } else if let Some(tok) = read_loopback_token() {
                req = req.header("authorization", format!("Bearer {tok}"));
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!(
                    "local daemon returned {status} for GET {path}: {}",
                    text.trim()
                );
            }
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("decode GET {path} body: {e}"))
        })
    }
}
