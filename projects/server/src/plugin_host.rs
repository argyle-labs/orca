//! TCP + mTLS plugin host — accepts connections from local and remote plugins.
//!
//! Listens on `0.0.0.0:<APP_PLUGIN_PORT>` (default 12002). Requires client
//! certificates signed by the orca CA. Dispatches JSON-RPC 2.0 frames.
//!
//! Phase A methods:
//!   orca/hello  — version handshake; always responds ok for compatible SDKs

use anyhow::{Context, Result};
use orca_sdk::framing::{read_frame, write_frame};
use orca_sdk::jsonrpc::{ErrorObject, Message, Response};
use orca_sdk::pki;
use orca_sdk::transport::HelloParams;
use rustls::ServerConfig;
use rustls::server::WebPkiClientVerifier;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// Start the plugin host in a background task. Returns immediately.
/// If the PKI directory doesn't contain a CA, logs a warning and skips the host.
pub fn start(pki_dir: &Path, port: u16) -> tokio::task::JoinHandle<()> {
    let pki_dir = pki_dir.to_owned();
    tokio::spawn(async move {
        if let Err(e) = run(&pki_dir, port).await {
            warn!("[plugin-host] failed to start: {e:#}");
        }
    })
}

async fn run(pki_dir: &Path, port: u16) -> Result<()> {
    let server_bundle = pki::load_server(pki_dir).context("load server TLS bundle")?;
    let acceptor = build_acceptor(&server_bundle)?;

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("plugin host bind {addr}"))?;

    info!("[plugin-host] listening on {addr} (mTLS)");

    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor.accept(tcp).await {
                        Ok(tls) => {
                            if let Err(e) = handle_connection(tls).await {
                                warn!("[plugin-host] {peer} connection error: {e:#}");
                            }
                        }
                        Err(e) => warn!("[plugin-host] {peer} TLS accept failed: {e}"),
                    }
                });
            }
            Err(e) => warn!("[plugin-host] accept error: {e}"),
        }
    }
}

fn build_acceptor(bundle: &pki::NodeBundle) -> Result<TlsAcceptor> {
    let (cert_chain, private_key) = pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let ca_root_store = Arc::new(pki::ca_root_store(&bundle.ca_cert_pem)?);

    let client_cert_verifier = WebPkiClientVerifier::builder(ca_root_store)
        .build()
        .context("build client cert verifier")?;

    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_cert_verifier)
        .with_single_cert(cert_chain, private_key)
        .context("build server TLS config")?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

async fn handle_connection(
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(tls);

    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(f) => f,
            Err(e) => {
                // EOF or clean disconnect — not an error worth logging.
                let msg = e.to_string();
                if msg.contains("unexpected end of file") || msg.contains("early eof") {
                    break;
                }
                return Err(e);
            }
        };

        let response = dispatch(&frame);
        let response_bytes = serde_json::to_vec(&response)?;
        write_frame(&mut writer, &response_bytes).await?;
    }

    Ok(())
}

fn dispatch(frame: &[u8]) -> serde_json::Value {
    let msg: Message = match serde_json::from_slice(frame) {
        Ok(m) => m,
        Err(e) => {
            return json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            });
        }
    };

    match msg {
        Message::Request(req) => {
            let id = req.id.clone();
            match req.method.as_str() {
                "orca/hello" => handle_hello(id, req.params),
                other => {
                    serde_json::to_value(Response::err(id, ErrorObject::method_not_found(other)))
                        .expect("Response serializes")
                }
            }
        }
        // Notifications have no id — don't respond.
        Message::Notification(_) | Message::Response(_) => json!(null),
    }
}

fn handle_hello(id: serde_json::Value, params: Option<serde_json::Value>) -> serde_json::Value {
    let params: HelloParams = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return serde_json::to_value(Response::err(
                id,
                ErrorObject::invalid_params("orca/hello requires params"),
            ))
            .expect("Response serializes");
        }
    };

    info!(
        "[plugin-host] hello from plugin '{}' (sdk {}, flavor {:?})",
        params.plugin_id, params.sdk_version, params.flavor
    );

    let result = json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "ok": true,
        "status": "full",
        "methods": ["orca/hello"]
    });

    serde_json::to_value(Response::ok(id, result)).expect("Response serializes")
}
