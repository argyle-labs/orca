pub mod api;
pub mod mcp_client;
pub mod middleware;
mod openapi;
#[cfg(feature = "pdf")]
pub mod pdf_gen;
pub mod tree;

pub use openapi::orca_spec_json as openapi_spec_json;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use orca_utils::state::{DaemonMode, DaemonState};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

pub async fn run(dev: bool, port: u16, db_path: std::path::PathBuf) -> Result<()> {
    let pki_dir = db_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(orca_utils::config::APP_PKI_DIR);
    let app = build_router(dev, db_path);

    let addr: SocketAddr = if dev {
        format!("127.0.0.1:{port}").parse()?
    } else {
        format!("0.0.0.0:{port}").parse()?
    };

    info!("[orca] binding {}...", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        anyhow::anyhow!("failed to bind {addr}: {e} — is port {port} already in use?")
    })?;
    info!("[orca] listening on http://localhost:{port}");

    // Register as the active dev process so the parked daemon won't auto-reclaim.
    // Use ORCA_DEV_PARENT_PID (the shell script PID) so the registration stays
    // valid across cargo-watch rebuilds — the shell script outlives each server instance.
    if dev && let Ok(Some(s)) = orca_utils::state::read() {
        let active_pid = std::env::var("ORCA_DEV_PARENT_PID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(std::process::id);
        if let Err(e) = orca_utils::state::write(&DaemonState {
            mode: DaemonMode::Dev,
            active_pid,
            ..s
        }) {
            tracing::warn!("failed to write dev state: {e}");
        }
    }

    // Non-blocking update check — prints a notice if a newer version is available.
    tokio::spawn(crate::commands::startup_update_check());

    // Plugin host: TCP + mTLS on APP_PLUGIN_PORT. Skips gracefully if PKI not initialized.
    crate::plugin_host::start(
        &pki_dir,
        orca_utils::config::APP_PLUGIN_PORT,
        crate::plugin_host::PluginRegistry::new(),
    );

    axum::serve(listener, app).await?;
    Ok(())
}

/// Daemon serve loop with cooperative port handoff via UNIX signals.
///
/// SIGUSR1 → drop listener (release port), write mode=parked, wait.
/// SIGUSR2 → rebind port, write mode=daemon, resume serving.
/// SIGTERM / Ctrl-C → clean shutdown, remove state file.
///
/// While parked, polls every 5 s: if the active dev process has died,
/// auto-reclaims the port without waiting for a signal.
pub async fn run_daemon(port: u16, db_path: std::path::PathBuf) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let pki_dir = db_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(orca_utils::config::APP_PKI_DIR);
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let app = build_router(false, db_path);

    let binary = resolve_daemon_binary();

    if let Err(e) = orca_utils::state::write(&DaemonState {
        daemon_pid: std::process::id(),
        active_pid: std::process::id(),
        port,
        mode: DaemonMode::Daemon,
        binary,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: chrono::Utc::now(),
    }) {
        tracing::warn!("failed to write initial daemon state: {e}");
    }

    // Plugin host: TCP + mTLS. Skips gracefully if PKI not initialized.
    crate::plugin_host::start(
        &pki_dir,
        orca_utils::config::APP_PLUGIN_PORT,
        crate::plugin_host::PluginRegistry::new(),
    );

    let mut sigterm = signal(SignalKind::terminate())?;

    // Crash-restart recovery: if launchd restarted us while a dev session was active,
    // wait for the dev server to finish rather than immediately fighting it for the port.
    if let Ok(Some(mut s)) = orca_utils::state::read()
        && s.mode == DaemonMode::Dev
    {
        info!("[orca] restarted while dev session active — waiting for dev to exit");
        s.daemon_pid = std::process::id();
        if let Err(e) = orca_utils::state::write(&s) {
            tracing::warn!("failed to update daemon_pid in state: {e}");
        }

        // Register SIGUSR2 now so dev can signal us at the new PID
        let mut sigusr2 = signal(SignalKind::user_defined2())?;
        loop {
            tokio::select! {
                _ = sigusr2.recv() => break,
                _ = sigterm.recv() => {
                    let _ = orca_utils::state::clear();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    if let Ok(Some(s)) = orca_utils::state::read() {
                        if s.mode != DaemonMode::Dev || !pid_alive(s.active_pid) { break; }
                    } else {
                        break;
                    }
                }
            }
        }
        info!("[orca] dev session ended — binding port {port}");
    }

    loop {
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            anyhow::anyhow!("failed to bind {addr}: {e} — is port {port} already in use?")
        })?;
        info!("[orca] daemon listening on http://localhost:{port}");
        if let Err(e) = orca_utils::state::set_mode(DaemonMode::Daemon) {
            tracing::warn!("failed to set daemon mode: {e}");
        }
        if let Err(e) = orca_utils::state::set_active_pid(std::process::id()) {
            tracing::warn!("failed to set active_pid: {e}");
        }

        let mut sigusr1 = signal(SignalKind::user_defined1())?;

        let parked = tokio::select! {
            result = axum::serve(listener, app.clone()) => { result?; false }
            _ = sigusr1.recv() => true,
            _ = sigterm.recv() => {
                info!("[orca] daemon shutting down");
                let _ = orca_utils::state::clear();
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                info!("[orca] daemon shutting down");
                let _ = orca_utils::state::clear();
                return Ok(());
            }
        };

        if !parked {
            break;
        }

        // A5 fix: register SIGUSR2 handler BEFORE writing Parked to state.
        // Default SIGUSR2 disposition is process termination — if the signal
        // arrives between set_mode(Parked) and the handler registration it kills us.
        let mut sigusr2 = signal(SignalKind::user_defined2())?;

        // Port released (listener dropped by select! cancellation)
        if let Err(e) = orca_utils::state::set_mode(DaemonMode::Parked) {
            tracing::warn!("failed to set parked mode: {e}");
        }
        info!("[orca] daemon parked — port {port} released");

        loop {
            tokio::select! {
                _ = sigusr2.recv() => {
                    info!("[orca] daemon reclaiming port {port}");
                    break;
                }
                _ = sigterm.recv() => {
                    info!("[orca] daemon shutting down (while parked)");
                    let _ = orca_utils::state::clear();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Auto-reclaim if dev process died OR nobody ever took the port
                    if let Ok(Some(s)) = orca_utils::state::read() {
                        let abandoned = match s.mode {
                            DaemonMode::Dev => !pid_alive(s.active_pid),
                            // Parked with active_pid still pointing at daemon → dev never started
                            DaemonMode::Parked => s.active_pid == s.daemon_pid,
                            DaemonMode::Daemon => false,
                        };
                        if abandoned {
                            info!("[orca] auto-reclaiming port {port} (dev abandoned)");
                            break;
                        }
                    }
                }
            }
        }
        // Outer loop: rebind and serve again
    }

    let _ = orca_utils::state::clear();
    Ok(())
}

/// Serve the Scalar API reference viewer.
/// The SvelteKit `routes/scalar/+server.ts` is SSR-only and doesn't survive
/// the prerendered static build embedded in the orca binary. This handler
/// replaces it, serving the same Scalar HTML with the spec URL from ?url=.
async fn scalar_handler(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{Response, header};

    let spec_url = params
        .get("url")
        .cloned()
        .unwrap_or_else(|| "/api/openapi.json".to_string());

    let html = format!(
        r#"<!doctype html>
<html>
<head>
  <title>API Reference</title>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <style>body {{ margin: 0; }}</style>
</head>
<body>
  <script id="api-reference" data-url="{spec_url}"></script>
  <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#
    );

    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .expect("hardcoded headers are valid")
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns the path to the orca binary suitable for respawning after a redeploy.
/// Prefers `which orca` (the symlink on PATH) over `current_exe()` (the canonical
/// resolved path). After a redeploy, the symlink is updated to the new binary;
/// the canonical path from current_exe() points to the old binary on disk.
fn resolve_daemon_binary() -> String {
    if let Ok(out) = std::process::Command::new("which").arg("orca").output()
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            return path;
        }
    }
    std::env::current_exe()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

// Embedded web UI — compiled in by default (the `ui` Cargo feature). True
// headless builds disable it with `--no-default-features`. Independently, the
// `ui.enabled` DB setting toggles serving at runtime (default true, read once
// at daemon startup — flip + restart to change).
#[cfg(feature = "ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../frontend/dist/"]
struct Assets;

#[cfg(feature = "ui")]
static UI_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

#[cfg(feature = "ui")]
fn ui_enabled() -> bool {
    *UI_ENABLED.get_or_init(|| {
        // Resolve through `db::open_default()` so we honour the same task-
        // local / env-var / encrypted-default resolution path as every other
        // handler. Doing this lazily on first request (not in build_router)
        // avoids forcing the production encrypted-open codepath on tests
        // that supplied an unencrypted DB via task-local.
        let enabled = db::open_default()
            .ok()
            .and_then(|c| db::settings::get(&c, "ui.enabled").ok().flatten())
            .map(|v| v != "false")
            .unwrap_or(true);
        tracing::info!("ui.enabled = {enabled}");
        enabled
    })
}

#[cfg(feature = "ui")]
async fn static_handler(uri: axum::http::Uri) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{Response, header};

    if !ui_enabled() {
        return Response::builder()
            .status(404)
            .header("content-type", "text/plain")
            .body(Body::from(
                "web UI disabled — set settings.ui.enabled = 'true' and restart",
            ))
            .expect("hardcoded response is valid");
    }

    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data))
                .expect("mime type is a valid header value")
        }
        // SPA: any unmatched path serves index.html so client-side routing handles it.
        None => match Assets::get("index.html") {
            Some(content) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(content.data))
                .expect("hardcoded headers are valid"),
            None => Response::builder()
                .status(404)
                .body(Body::empty())
                .expect("404 response is valid"),
        },
    }
}

#[cfg(not(feature = "ui"))]
async fn static_handler(_uri: axum::http::Uri) -> axum::response::Response {
    use axum::body::Body;
    axum::http::Response::builder()
        .status(404)
        .header("content-type", "text/plain")
        .body(Body::from(
            "headless build — rebuild without --no-default-features to embed the UI",
        ))
        .expect("hardcoded response is valid")
}

// ── Dev proxy ─────────────────────────────────────────────────────────────────
// In dev mode, Rust owns port 12000 and proxies non-API requests to Vite at
// :12001. This means the browser always uses one port for both API and UI,
// matching the prod layout exactly.

const VITE_ORIGIN: &str = "http://127.0.0.1:12001";
const VITE_WS_ORIGIN: &str = "ws://127.0.0.1:12001";

// Hop-by-hop headers that must not be forwarded.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
    )
}

async fn dev_proxy_handler(req: axum::extract::Request) -> axum::response::Response {
    use axum::extract::ws::WebSocketUpgrade;

    let is_ws = req
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_ws {
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        use axum::extract::FromRequest;
        use axum::response::IntoResponse;
        return match WebSocketUpgrade::from_request(req, &()).await {
            Ok(ws) => ws.on_upgrade(move |sock| proxy_ws_to_vite(sock, path)),
            Err(e) => e.into_response(),
        };
    }

    proxy_http_to_vite(req).await
}

async fn proxy_http_to_vite(req: axum::extract::Request) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::Response;

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let url = format!("{VITE_ORIGIN}{path_and_query}");

    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    let client = reqwest::Client::new();
    let mut rb = client.request(method, &url);

    for (k, v) in req.headers() {
        if is_hop_by_hop(k.as_str()) || k == axum::http::header::HOST {
            continue;
        }
        rb = rb.header(k.as_str(), v);
    }

    let body = axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap_or_default();
    rb = rb.body(body);

    match rb.send().await {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let mut builder = Response::builder().status(status);
            for (k, v) in resp.headers() {
                if is_hop_by_hop(k.as_str()) {
                    continue;
                }
                builder = builder.header(k.as_str(), v);
            }
            let bytes = resp.bytes().await.unwrap_or_default();
            builder.body(Body::from(bytes)).unwrap_or_else(|_| {
                Response::builder()
                    .status(502)
                    .body(Body::empty())
                    .expect("502 response is valid")
            })
        }
        Err(_) => Response::builder()
            .status(502)
            .body(Body::from(
                "orca: vite dev server unreachable — is it running on :12001?",
            ))
            .expect("502 response is valid"),
    }
}

async fn proxy_ws_to_vite(mut browser: axum::extract::ws::WebSocket, path: String) {
    use axum::extract::ws::Message as BMsg;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message as VMsg};

    let url = format!("{VITE_WS_ORIGIN}{path}");
    let (mut vite, _) = match connect_async(&url).await {
        Ok(v) => v,
        Err(_) => {
            let _ = browser.close().await;
            return;
        }
    };

    loop {
        tokio::select! {
            msg = browser.recv() => match msg {
                Some(Ok(BMsg::Text(t)))   => { let _ = vite.send(VMsg::Text(t.as_str().into())).await; }
                Some(Ok(BMsg::Binary(b))) => { let _ = vite.send(VMsg::Binary(b.to_vec().into())).await; }
                Some(Ok(BMsg::Ping(p)))   => { let _ = vite.send(VMsg::Ping(p.to_vec().into())).await; }
                Some(Ok(BMsg::Pong(p)))   => { let _ = vite.send(VMsg::Pong(p.to_vec().into())).await; }
                _ => break,
            },
            msg = vite.next() => match msg {
                Some(Ok(VMsg::Text(t)))   => { let _ = browser.send(BMsg::Text(t.as_str().into())).await; }
                Some(Ok(VMsg::Binary(b))) => { let _ = browser.send(BMsg::Binary(b.to_vec().into())).await; }
                Some(Ok(VMsg::Ping(p)))   => { let _ = browser.send(BMsg::Ping(p.to_vec().into())).await; }
                Some(Ok(VMsg::Pong(p)))   => { let _ = browser.send(BMsg::Pong(p.to_vec().into())).await; }
                _ => break,
            },
        }
    }

    let _ = browser.close().await;
}

/// Build the axum `Router` — exposed so integration tests can call it directly.
pub fn build_router(dev: bool, db_path: std::path::PathBuf) -> Router {
    use std::sync::Arc;

    // Ensures reqwest (rustls-no-provider) has a crypto provider; idempotent.
    crate::llm::ensure_crypto_provider();

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mcp_pool = Arc::new(mcp_client::McpPool::new_with_db(db_path));

    let (api, spec) = openapi::openapi_router().split_for_parts();
    // Stash the assembled spec so the spec-serving handlers can read it.
    openapi::install_spec(spec);
    // Write orca's own spec to disk so it lives alongside rebuy's scanner-generated specs.
    write_orca_spec_to_disk();

    let api = api
        // Spec endpoints — registered after split so they are not themselves
        // documented in the spec (would be circular and noisy).
        .route("/api/openapi.json", get(openapi::openapi_handler))
        .route(
            "/api/openapi/public.json",
            get(openapi::openapi_public_handler),
        )
        // Scalar API reference viewer — served by Rust so it works in the
        // prerendered static build (SvelteKit SSR routes don't survive embedding).
        .route("/scalar", get(scalar_handler))
        .with_state(mcp_pool)
        .layer(axum::middleware::from_fn(middleware::log_requests))
        .layer(cors);

    // Mount the OrcaTool registry under /api/tools. Same registry as MCP stdio
    // and CLI — one trait impl, three live surfaces (REST + MCP + CLI).
    let api = match orca_utils::config::Config::load() {
        Ok(cfg) => {
            // Reuse the same registry + service-trait setup that the CLI and
            // MCP-stdio surfaces use, otherwise tools that look up services on
            // ToolCtx (lifecycle, profile, pki, etc.) return 500.
            let cfg = Arc::new(cfg);
            let (reg, ctx) = crate::mcp::build_tool_registry(cfg);
            api.nest("/api/tools", Arc::new(reg).axum_router(Arc::new(ctx)))
        }
        Err(e) => {
            tracing::warn!("Config::load failed, /api/tools disabled: {e}");
            api
        }
    };

    if dev {
        api.fallback(dev_proxy_handler)
    } else {
        api.fallback(static_handler)
    }
}

/// Write orca's generated OpenAPI spec to ~/.orca/specs/orca.json so it
/// lives alongside rebuy's scanner-generated specs and can be compared to them.
fn write_orca_spec_to_disk() {
    let dir = crate::scanner::specs_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not create openapi dir {}: {e}", dir.display());
        return;
    }
    let path = dir.join("orca.json");
    let spec = openapi::orca_spec_json();
    match serde_json::to_string_pretty(&spec) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("could not write orca spec to {}: {e}", path.display());
            }
        }
        Err(e) => tracing::warn!("could not serialize orca spec: {e}"),
    }
}
