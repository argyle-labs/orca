pub mod api;
pub mod mcp_client;
pub mod middleware;
mod openapi;
pub mod tree;

pub use openapi::brain_spec_json as openapi_spec_json;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::routing::{get, post};
use brain_utils::state::{self, DaemonMode, DaemonState};
use tower_http::cors::{Any, CorsLayer};

pub async fn run(dev: bool, port: u16, mcp_servers: Vec<brain_utils::config::McpServerEntry>) -> Result<()> {
    let app = build_router(dev, mcp_servers);

    let addr: SocketAddr = if dev {
        format!("127.0.0.1:{port}").parse()?
    } else {
        format!("0.0.0.0:{port}").parse()?
    };

    println!("[brain] binding {}...", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        anyhow::anyhow!("failed to bind {addr}: {e} — is port {port} already in use?")
    })?;
    println!("[brain] listening on http://localhost:{port}");

    // Non-blocking update check — prints a notice if a newer version is available.
    tokio::spawn(brain_commands::startup_update_check());

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
pub async fn run_daemon(port: u16, mcp_servers: Vec<brain_utils::config::McpServerEntry>) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let app = build_router(false, mcp_servers);

    let binary = resolve_daemon_binary();

    let _ = state::write(&DaemonState {
        daemon_pid: std::process::id(),
        active_pid: std::process::id(),
        port,
        mode: DaemonMode::Daemon,
        binary,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: chrono::Utc::now(),
    });

    let mut sigterm = signal(SignalKind::terminate())?;

    // Crash-restart recovery: if launchd restarted us while a dev session was active,
    // wait for the dev server to finish rather than immediately fighting it for the port.
    if let Ok(Some(mut s)) = state::read() {
        if s.mode == DaemonMode::Dev {
            println!("[brain] restarted while dev session active — waiting for dev to exit");
            s.daemon_pid = std::process::id();
            let _ = state::write(&s);

            // Register SIGUSR2 now so dev can signal us at the new PID
            let mut sigusr2 = signal(SignalKind::user_defined2())?;
            loop {
                tokio::select! {
                    _ = sigusr2.recv() => break,
                    _ = sigterm.recv() => {
                        let _ = state::clear();
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        if let Ok(Some(s)) = state::read() {
                            if s.mode != DaemonMode::Dev || !pid_alive(s.active_pid) { break; }
                        } else {
                            break;
                        }
                    }
                }
            }
            println!("[brain] dev session ended — binding port {port}");
        }
    }

    loop {
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            anyhow::anyhow!("failed to bind {addr}: {e} — is port {port} already in use?")
        })?;
        println!("[brain] daemon listening on http://localhost:{port}");
        let _ = state::set_mode(DaemonMode::Daemon);
        let _ = state::set_active_pid(std::process::id());

        let mut sigusr1 = signal(SignalKind::user_defined1())?;

        let parked = tokio::select! {
            result = axum::serve(listener, app.clone()) => { result?; false }
            _ = sigusr1.recv() => true,
            _ = sigterm.recv() => {
                println!("[brain] daemon shutting down");
                let _ = state::clear();
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[brain] daemon shutting down");
                let _ = state::clear();
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
        let _ = state::set_mode(DaemonMode::Parked);
        println!("[brain] daemon parked — port {port} released");

        loop {
            tokio::select! {
                _ = sigusr2.recv() => {
                    println!("[brain] daemon reclaiming port {port}");
                    break;
                }
                _ = sigterm.recv() => {
                    println!("[brain] daemon shutting down (while parked)");
                    let _ = state::clear();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Auto-reclaim if dev process died OR nobody ever took the port
                    if let Ok(Some(s)) = state::read() {
                        let abandoned = match s.mode {
                            DaemonMode::Dev => !pid_alive(s.active_pid),
                            // Parked with active_pid still pointing at daemon → dev never started
                            DaemonMode::Parked => s.active_pid == s.daemon_pid,
                            DaemonMode::Daemon => false,
                        };
                        if abandoned {
                            println!("[brain] auto-reclaiming port {port} (dev abandoned)");
                            break;
                        }
                    }
                }
            }
        }
        // Outer loop: rebind and serve again
    }

    let _ = state::clear();
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns the path to the brain binary suitable for respawning after a redeploy.
/// Prefers `which brain` (the symlink on PATH) over `current_exe()` (the canonical
/// resolved path). After a redeploy, the symlink is updated to the new binary;
/// the canonical path from current_exe() points to the old binary on disk.
fn resolve_daemon_binary() -> String {
    if let Ok(out) = std::process::Command::new("which").arg("brain").output() {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                return path;
            }
        }
    }
    std::env::current_exe()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

// frontend/dist is compiled into the binary at build time so the binary ships alone —
// no separate frontend/ directory needed at the install destination.
#[derive(rust_embed::RustEmbed)]
#[folder = "../frontend/dist/"]
struct Assets;

async fn static_handler(uri: axum::http::Uri) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{Response, header};

    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data))
                .unwrap()
        }
        // SPA: any unmatched path serves index.html so client-side routing handles it.
        None => match Assets::get("index.html") {
            Some(content) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(content.data))
                .unwrap(),
            None => Response::builder().status(404).body(Body::empty()).unwrap(),
        },
    }
}

fn build_router(dev: bool, mcp_servers: Vec<brain_utils::config::McpServerEntry>) -> Router {
    use std::sync::Arc;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mcp_pool = Arc::new(mcp_client::McpPool::new(mcp_servers));

    let api = Router::new()
        .route("/api/health", get(api::ping_handler))
        // brain's own spec — separate from the external registry
        .route("/api/openapi.json", get(openapi::openapi_handler))
        .route(
            "/api/openapi/public.json",
            get(openapi::openapi_public_handler),
        )
        // external repo spec registry
        .route("/api/specs", get(api::specs_list_handler))
        .route(
            "/api/specs/:repo/public",
            get(api::specs_get_public_handler),
        )
        .route(
            "/api/specs/:repo/graphql/info",
            get(api::specs_graphql_info_handler),
        )
        .route(
            "/api/specs/:repo/graphql",
            get(api::specs_get_graphql_handler),
        )
        .route("/api/specs/:repo", get(api::specs_get_handler))
        .route("/api/tree", get(api::tree_handler))
        .route("/api/search", get(api::search_handler))
        .route("/api/mcp/tools", get(api::mcp_tools_handler))
        .route("/api/mcp/run", post(api::mcp_run_handler))
        .route("/api/docker/engine", get(api::docker_engine_handler))
        .route("/api/docker/engine/start", post(api::docker_engine_start_handler))
        .route("/api/docker/services", get(api::docker_services_handler))
        .route("/api/docker/action", post(api::docker_action_handler))
        .route("/api/ctx7", get(api::ctx7_handler))
        .route("/api/doc", get(api::doc_handler))
        .route("/api/schema", get(api::schema_handler))
        .route("/api/schema/domains", get(api::schema_domains_handler))
        .route("/api/rebuy/health/local", get(api::rebuy_health_handler))
        .route("/api/logs/services", get(api::log_services_handler))
        .route("/api/logs", get(api::log_fetch_handler))
        .route("/api/tests/run", get(api::tests_run_handler))
        .route("/api/bitbucket/repos", get(api::repos_handler))
        .route("/api/bitbucket/prs", get(api::prs_handler))
        .route("/api/jira/issues", get(api::jira_issues_handler))
        .route("/api/jira/issues/:key/transitions", get(api::jira_get_transitions_handler))
        .route("/api/jira/issues/:key/transitions", post(api::jira_transition_handler))
        .route("/api/confluence/search", get(api::confluence_search_handler))
        .route("/api/system/status", get(api::system_status_handler))
        .route("/api/system/action", post(api::system_action_handler))
        .with_state(mcp_pool)
        .layer(axum::middleware::from_fn(middleware::log_requests))
        .layer(cors);

    if dev {
        api
    } else {
        api.fallback(static_handler)
    }
}
