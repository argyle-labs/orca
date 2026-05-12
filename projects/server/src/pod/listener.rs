//! Server-side handler for SNI=pod.orca.local connections. Reads JSON-RPC
//! frames, dispatches recognized pod/* methods, writes responses.
//!
//! For v1 this is a stateless single-method server (`pod/ping`). Future
//! phases will add join, list, trust transitions.

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Response};
use tokio_rustls::server::TlsStream;
use tracing::warn;

use super::{POD_PING_METHOD, PodPingResult};

/// Handle a single pod connection. v1 expects exactly one JSON-RPC frame
/// (pod/ping), responds, and closes. We don't keep these open since pod
/// traffic in v1 is one-shot ping; the join workflow in v2 will be the same
/// shape (one request, one response).
pub async fn handle_pod_connection(
    mut tls: TlsStream<tokio::net::TcpStream>,
    peer_cn: String,
) -> Result<()> {
    let frame_bytes = read_frame(&mut tls).await.context("read pod frame")?;
    let msg: Message =
        serde_json::from_slice(&frame_bytes).context("parse pod frame as JSON-RPC")?;
    let request = match msg {
        Message::Request(r) => r,
        Message::Response(_) | Message::Notification(_) => {
            warn!("[pod] {peer_cn} sent non-request frame; closing");
            return Ok(());
        }
    };

    let response = match request.method.as_str() {
        POD_PING_METHOD => {
            let result = PodPingResult {
                peer_id: peer_cn.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                hostname: gethostname_string(),
            };
            Response::ok(request.id, serde_json::to_value(result)?)
        }
        other => Response::err(
            request.id,
            ErrorObject::method_not_found(&format!("pod method '{other}' not supported in v1")),
        ),
    };

    let envelope = serde_json::to_vec(&response).context("serialize pod response")?;
    write_frame(&mut tls, &envelope)
        .await
        .context("write pod response")?;
    Ok(())
}

fn gethostname_string() -> String {
    // Best-effort; falls back to "unknown" rather than failing the response.
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
