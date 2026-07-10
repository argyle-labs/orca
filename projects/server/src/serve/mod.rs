pub mod auth_routes;
pub mod middleware;
pub mod openapi;
#[cfg(feature = "pdf")]
pub mod pdf_gen;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::FromRequest;
use axum::http::{HeaderName, Method};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum_server::tls_rustls::RustlsConfig;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::info;
use utils::state::{DaemonMode, DaemonState};

/// Hard ceiling on graceful shutdown. If background + in-flight tasks haven't
/// drained within this budget, the process force-exits rather than hanging a
/// service-manager stop indefinitely. Generous enough to let an in-flight peer
/// tool-call or a final DB flush finish; short enough that a wedged task can't
/// block a deploy.
const GLOBAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Drain background + in-flight tasks under the global timeout. Cancels the
/// shutdown token, closes the tracker, and waits for tracked work (mesh
/// in-flight peer tool-calls, etc.) to complete. Returns whether everything
/// drained within budget; the caller logs a warning and proceeds either way —
/// `state::clear()` must run AFTER this so on-disk state is released only once
/// flushes have had their chance.
async fn drain_with_timeout() {
    if !utils::shutdown::drain(GLOBAL_SHUTDOWN_TIMEOUT).await {
        tracing::warn!(
            "[orca] shutdown drain exceeded {}s budget — proceeding with forced exit",
            GLOBAL_SHUTDOWN_TIMEOUT.as_secs()
        );
    }
}

/// Guard for `--dev`: refuse if more than one user is registered.
/// Plain-HTTP + relaxed cookie attrs are only safe on a single-user host.
pub(crate) fn dev_multi_user_guard(users: i64) -> Result<()> {
    if users > 1 {
        anyhow::bail!(
            "--dev refused: {users} users registered. Plain-HTTP + relaxed cookie attrs are only safe on a single-user host."
        );
    }
    Ok(())
}

/// Publish the HTTP port this instance actually bound to a runtime hint file
/// (`$ORCA_HOME/http.port`). The CLI can't depend on `db`/`files` (dependency
/// cycle), so it can't resolve the per-instance DB-persisted port itself — it
/// reads this file to dial the right port when an instance runs on a non-default
/// port set via the DB rather than `ORCA_HTTP_PORT`. Best-effort: a write
/// failure only means the CLI falls back to env/const resolution.
fn publish_http_port(port: u16) {
    let Some(home) = files::ops::orca_home() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&home)
        .and_then(|()| std::fs::write(home.join("http.port"), port.to_string()))
    {
        tracing::warn!(
            "could not publish http port hint to {}: {e}",
            home.display()
        );
    }
}

pub async fn run(dev: bool, port: u16, db_path: std::path::PathBuf) -> Result<()> {
    // Prod guard for `--dev`: drops `Secure` cookie, serves plain HTTP, and
    // relaxes SameSite. Safe on a single-user laptop; unsafe the moment a
    // multi-user host adopts it. Refuse if more than one user exists.
    if dev {
        let conn = db::open(&db_path)
            .with_context(|| format!("open {} for --dev guard", db_path.display()))?;
        let users = db::users::count(&conn).context("count users for --dev guard")?;
        drop(conn);
        dev_multi_user_guard(users)?;
    }
    let pki_dir = db_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(contract::config::APP_PKI_DIR);
    let app = build_router(dev, db_path);

    let addr: SocketAddr = if dev {
        format!("127.0.0.1:{port}").parse()?
    } else {
        format!("0.0.0.0:{port}").parse()?
    };
    publish_http_port(port);

    // In dev we serve plain HTTP — no self-signed cert ordeal, browsers
    // happily store cookies, http://localhost:12000 "just works". Production
    // still gets TLS.
    let tls = if dev {
        None
    } else {
        Some(load_rest_tls(&pki_dir).await?)
    };
    info!(
        "[orca] binding {} ({})...",
        addr,
        if dev { "http" } else { "https" }
    );

    // Register as the active dev process so the parked daemon won't auto-reclaim.
    // Use ORCA_DEV_PARENT_PID (the shell script PID) so the registration stays
    // valid across cargo-watch rebuilds — the shell script outlives each server instance.
    if dev && let Ok(Some(s)) = utils::state::read() {
        let active_pid = std::env::var("ORCA_DEV_PARENT_PID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(std::process::id);
        if let Err(e) = utils::state::write(&DaemonState {
            mode: DaemonMode::Dev,
            active_pid,
            ..s
        }) {
            tracing::warn!("failed to write dev state: {e}");
        }
    }

    spawn_all_runtime_tasks(&pki_dir).await;

    let scheme = if dev { "http" } else { "https" };
    info!("[orca] listening on {scheme}://localhost:{port}");

    // Serve with cooperative shutdown: SIGTERM / Ctrl-C cancel the shutdown
    // token, drain background + in-flight tasks, then return. Without this the
    // foreground `run()` path left every background loop to be aborted
    // mid-await when the process was killed.
    let handle = axum_server::Handle::new();
    let server = async {
        match tls {
            Some(tls) => {
                axum_server::bind_rustls(addr, tls)
                    .handle(handle.clone())
                    .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .await
            }
            None => {
                axum_server::bind(addr)
                    .handle(handle.clone())
                    .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .await
            }
        }
    };

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = server => result?,
        _ = sigterm.recv() => {
            info!("[orca] shutting down");
            system::periodic::shutdown();
            handle.graceful_shutdown(Some(Duration::from_secs(1)));
            drain_with_timeout().await;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("[orca] shutting down");
            system::periodic::shutdown();
            handle.graceful_shutdown(Some(Duration::from_secs(1)));
            drain_with_timeout().await;
        }
    }
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

    // If we restarted because of an in-progress update, verify the swap took
    // and clear the marker. `system.update` surfaces the marker on stale
    // daemons so a remote probe can tell apply-but-no-restart apart from
    // apply-and-restarted.
    if let Some((target, age)) = system::update::read_pending_restart() {
        let running = env!("CARGO_PKG_VERSION");
        if target.trim_start_matches('v') == running {
            tracing::info!("[update] restart verified: now running v{running}");
            system::update::clear_pending_restart();
        } else {
            tracing::warn!(
                "[update] pending_restart marker present: target={target} running={running} age_secs={age}"
            );
        }
    }

    let pki_dir = db_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(contract::config::APP_PKI_DIR);
    // `port` is the HTTP bind, already resolved per-instance by the caller
    // (explicit `--port` > env `ORCA_HTTP_PORT` > persisted DB port > const).
    // HTTPS uses the Config-resolved https port (same precedence). Both
    // listen concurrently — homelab clients without an internal CA reach
    // http://host:<port> while internal mesh traffic and Caddy fronts
    // dial https://host:<https_port>.
    let ports = db::ports::current();
    publish_http_port(port);
    let http_addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let https_addr: SocketAddr = format!("0.0.0.0:{}", ports.https).parse()?;
    let app = build_router(false, db_path);

    let binary = resolve_daemon_binary();

    // If we were spawned by cargo-watch (cmd_dev_enable), the production daemon
    // is parked. Don't overwrite its daemon_pid; we'll re-park it below and register
    // ourselves as the active dev process before binding.
    // Primary signal: env-var set by `pod dev_enable` when it spawns cargo-watch.
    // Fallback: detect when this binary itself lives under `target/debug/` — that's
    // the unambiguous footprint of `cargo run` / `cargo watch`, and catches legacy
    // cargo-watch instances that pre-date the env-var convention.
    let dev_spawn = std::env::var("ORCA_DEV_PARENT_PID").is_ok() || spawned_by_cargo_watch();

    if dev_spawn {
        if let Ok(Some(mut s)) = utils::state::read() {
            // If production thinks it's still in Daemon mode, send SIGUSR1 to park.
            if matches!(s.mode, DaemonMode::Daemon) {
                tracing::info!(
                    "[dev] re-parking production daemon (pid {}) before bind",
                    s.daemon_pid
                );
                _ = std::process::Command::new("kill")
                    .args(["-USR1", &s.daemon_pid.to_string()])
                    .status();
                for _ in 0..30 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if let Ok(Some(s2)) = utils::state::read()
                        && matches!(s2.mode, DaemonMode::Parked)
                    {
                        s = s2;
                        break;
                    }
                }
                // Production released :12000 but its plugin host on :12002 is
                // independent — poll until :12002 actually frees so our bind
                // below doesn't race the prior listener's TCP teardown.
                for _ in 0..50 {
                    if tokio::net::TcpListener::bind(("0.0.0.0", ports.mesh))
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            s.active_pid = std::process::id();
            s.mode = DaemonMode::Dev;
            if let Err(e) = utils::state::write(&s) {
                tracing::warn!("failed to update dev state: {e}");
            }
        } else {
            // No prior state on disk (first dev spawn or state cleared).
            // Initialize a Dev state so peers' `pod/dev-sync` sees we're in
            // dev mode after cargo-watch respawns us across rebuilds.
            let parent_pid: u32 = std::env::var("ORCA_DEV_PARENT_PID")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(std::process::id);
            if let Err(e) = utils::state::write(&DaemonState {
                daemon_pid: std::process::id(),
                active_pid: parent_pid,
                port,
                mode: DaemonMode::Dev,
                binary: binary.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                started_at: utils::time::now(),
            }) {
                tracing::warn!("failed to initialize dev state: {e}");
            }
        }

        // Simple dev-binary serve loop: bind both HTTP + HTTPS, exit on SIGTERM.
        // Production daemon will reclaim ports when we exit.
        let tls = load_rest_tls(&pki_dir).await?;
        info!(
            "[orca] dev binary listening on http://localhost:{port} + https://localhost:{}",
            ports.https
        );

        spawn_all_runtime_tasks(&pki_dir).await;

        let mut sigterm = signal(SignalKind::terminate())?;
        let https_handle = axum_server::Handle::new();
        let http_handle = axum_server::Handle::new();
        let https_serve = axum_server::bind_rustls(https_addr, tls)
            .handle(https_handle.clone())
            .serve(
                app.clone()
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            );
        let http_serve = axum_server::bind(http_addr)
            .handle(http_handle.clone())
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>());
        tokio::select! {
            result = https_serve => result?,
            result = http_serve => result?,
            _ = sigterm.recv() => {
                system::periodic::shutdown();
                https_handle.graceful_shutdown(Some(Duration::from_secs(1)));
                http_handle.graceful_shutdown(Some(Duration::from_secs(1)));
                drain_with_timeout().await;
            }
        }
        return Ok(());
    }

    // Install the signal handlers BEFORE state.json reaches mode=Daemon.
    // The state file is the readiness gate operators (and the daemon test)
    // watch on, then immediately send USR1/USR2/TERM. tokio installs the
    // process-wide handler lazily on first `signal()` for a given kind; until
    // then the signal keeps its default disposition — and SIGUSR1/USR2's
    // default is *terminate*. Registering after the mode=Daemon write left a
    // race window (seen flaking under saturated parallel test load) where a
    // USR1 arriving in that gap killed the daemon instead of parking it. The
    // same receivers are reused inside the serve loop for park/reclaim.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    if let Err(e) = utils::state::write(&DaemonState {
        daemon_pid: std::process::id(),
        active_pid: std::process::id(),
        port,
        mode: DaemonMode::Daemon,
        binary,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: utils::time::now(),
    }) {
        tracing::warn!("failed to write initial daemon state: {e}");
    }

    spawn_all_runtime_tasks(&pki_dir).await;

    // Crash-restart recovery: if launchd restarted us while a dev session was active,
    // wait for the dev server to finish rather than immediately fighting it for the port.
    if let Ok(Some(mut s)) = utils::state::read()
        && s.mode == DaemonMode::Dev
    {
        info!("[orca] restarted while dev session active — waiting for dev to exit");
        s.daemon_pid = std::process::id();
        if let Err(e) = utils::state::write(&s) {
            tracing::warn!("failed to update daemon_pid in state: {e}");
        }

        // Register SIGUSR2 now so dev can signal us at the new PID
        let mut sigusr2 = signal(SignalKind::user_defined2())?;
        loop {
            tokio::select! {
                _ = sigusr2.recv() => break,
                _ = sigterm.recv() => {
                    // Background tasks were already spawned (above) — signal
                    // and drain them rather than letting the runtime abort
                    // them mid-await on return.
                    system::periodic::shutdown();
                    drain_with_timeout().await;
                    _ = utils::state::clear();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    if let Ok(Some(s)) = utils::state::read() {
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
        let tls = load_rest_tls(&pki_dir).await?;
        info!(
            "[orca] daemon listening on http://localhost:{port} + https://localhost:{}",
            ports.https
        );
        if let Err(e) = utils::state::set_mode(DaemonMode::Daemon) {
            tracing::warn!("failed to set daemon mode: {e}");
        }
        if let Err(e) = utils::state::set_active_pid(std::process::id()) {
            tracing::warn!("failed to set active_pid: {e}");
        }

        let https_handle = axum_server::Handle::new();
        let http_handle = axum_server::Handle::new();
        let https_serve = axum_server::bind_rustls(https_addr, tls)
            .handle(https_handle.clone())
            .serve(
                app.clone()
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            );
        let http_serve = axum_server::bind(http_addr)
            .handle(http_handle.clone())
            .serve(
                app.clone()
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            );

        let parked = tokio::select! {
            result = https_serve => { result?; false }
            result = http_serve  => { result?; false }
            _ = sigusr1.recv() => {
                // Park: drop both REST listeners so the dev binary can take
                // the port. Plugins are compile-time-linked now — no separate
                // host process to stop.
                https_handle.shutdown();
                http_handle.shutdown();
                true
            }
            _ = sigterm.recv() => {
                info!("[orca] daemon shutting down");
                system::periodic::shutdown();
                https_handle.graceful_shutdown(Some(Duration::from_secs(1)));
                http_handle.graceful_shutdown(Some(Duration::from_secs(1)));
                // Drain in-flight tasks BEFORE releasing on-disk state so a
                // final flush (mesh peer call, DB write) completes first.
                drain_with_timeout().await;
                _ = utils::state::clear();
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                info!("[orca] daemon shutting down");
                system::periodic::shutdown();
                https_handle.graceful_shutdown(Some(Duration::from_secs(1)));
                http_handle.graceful_shutdown(Some(Duration::from_secs(1)));
                drain_with_timeout().await;
                _ = utils::state::clear();
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
        if let Err(e) = utils::state::set_mode(DaemonMode::Parked) {
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
                    system::periodic::shutdown();
                    drain_with_timeout().await;
                    _ = utils::state::clear();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Auto-reclaim if dev process died OR nobody ever took the port
                    if let Ok(Some(s)) = utils::state::read() {
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

    _ = utils::state::clear();
    Ok(())
}

/// Load the REST API's TLS config from the local core CA's server cert.
/// Cert SAN is `core.orca.local` (no public domain baked in) — clients
/// either pin the core CA out-of-band, or a fronting proxy like Caddy
/// terminates a public cert and dials this listener over the local CA.
async fn load_rest_tls(pki_dir: &std::path::Path) -> Result<RustlsConfig> {
    // Auto-init on first boot: previously this returned a hard error if the
    // user hadn't run `orca install` yet, which also broke the daemon test
    // harness (fresh HOME, no PKI). Init is idempotent and cheap.
    if !utils::pki::ca_cert_path(pki_dir).exists()
        || !utils::pki::server_cert_path(pki_dir).exists()
    {
        utils::pki::init(pki_dir).context("auto-init core PKI for REST TLS")?;
    } else {
        // Pre-upgrade certs only had `core.orca.local` as SAN. Browsers
        // won't store cookies for `https://localhost:…` with a mismatched
        // hostname even after bypassing the self-signed-CA warning. Detect
        // and re-issue automatically so the daemon fixes itself on restart.
        let cert_pem =
            std::fs::read_to_string(utils::pki::server_cert_path(pki_dir)).unwrap_or_default();
        if !utils::pki::rest_server_cert_has_localhost_san(&cert_pem) {
            utils::pki::refresh_rest_server_cert(pki_dir)
                .context("refresh REST server cert to add localhost SAN")?;
            info!("[pki] REST server cert refreshed — localhost SAN added");
        } else if !utils::pki::rest_server_cert_is_browser_compatible(&cert_pem) {
            // Pre-rc.9 cert used an Ed25519 leaf key. Firefox/Chrome reject
            // Ed25519 in TLS server auth — re-issue with ECDSA P-256.
            utils::pki::refresh_rest_server_cert(pki_dir)
                .context("refresh REST server cert to ECDSA P-256 for browser compatibility")?;
            info!("[pki] REST server cert refreshed — Ed25519 → ECDSA P-256 (browser TLS)");
        }
    }
    let bundle = utils::pki::load_server(pki_dir).context("load REST TLS bundle")?;
    RustlsConfig::from_pem(bundle.cert_pem.into_bytes(), bundle.key_pem.into_bytes())
        .await
        .context("build rustls config from core server cert + key")
}

/// Serve the Scalar API reference viewer.
/// The SvelteKit `routes/scalar/+server.ts` is SSR-only and doesn't survive
/// the prerendered static build embedded in the orca binary. This handler
/// replaces it, serving the same Scalar HTML with the spec URL from ?url=.
/// Open probe used by the browser TokenGate to decide between the
/// one-click "create admin token" flow (loopback + zero tokens) and the
/// "paste an existing token" flow. Identity is the connection peer IP —
/// remote browsers always get `available=false`.
#[derive(serde::Serialize)]
struct BootstrapStatus {
    available: bool,
}

/// Open liveness probe consumed by the web UI's local-host card and any
/// external monitor. Unauthenticated (listed in middleware open paths) so a
/// down/locked daemon is still distinguishable from a healthy one.
#[derive(serde::Serialize)]
struct Health {
    ok: bool,
}

async fn ping_handler() -> axum::Json<Health> {
    axum::Json(Health { ok: true })
}

async fn bootstrap_status_handler(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
) -> axum::Json<BootstrapStatus> {
    let loopback = peer.ip().is_loopback();
    let no_tokens = match db::open_default() {
        Ok(conn) => db::api_tokens::count(&conn)
            .map(|n| n == 0)
            .unwrap_or(false),
        Err(_) => false,
    };
    axum::Json(BootstrapStatus {
        available: loopback && no_tokens,
    })
}

async fn scalar_handler(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let spec_url = params
        .get("url")
        .cloned()
        .unwrap_or_else(|| "/api/openapi.json".to_string());
    render_scalar(&spec_url, "API Reference")
}

fn render_scalar(spec_url: &str, title: &str) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{Response, header};
    // Inline sign-in widget. Scalar runs same-origin with the API, so the
    // browser auto-attaches the `orca_session` cookie to every "try it"
    // request once you sign in here. The widget lives in a fixed banner so
    // it's visible regardless of which operation is open. It calls
    // `/api/auth/web/signin` directly with `credentials: include` so the
    // cookie ends up in the browser jar even though Scalar's own fetcher
    // doesn't touch it. The "me" probe on load tells you whether an
    // existing cookie is still good without forcing a sign-in.
    let html = format!(
        r#"<!doctype html>
<html>
<head>
  <title>{title}</title>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <style>
    body {{ margin: 0; }}
    #orca-auth {{
      position: sticky; top: 0; z-index: 9999;
      display: flex; gap: 8px; align-items: center;
      padding: 6px 12px;
      background: #0f1117; color: #cdd6f4;
      border-bottom: 1px solid #313244;
      font: 13px/1.4 ui-sans-serif, system-ui, sans-serif;
    }}
    #orca-auth input {{
      background: #1e1e2e; color: #cdd6f4;
      border: 1px solid #45475a; border-radius: 4px;
      padding: 4px 8px; font: inherit;
    }}
    #orca-auth button {{
      background: #89b4fa; color: #11111b; border: 0;
      border-radius: 4px; padding: 4px 12px; font: inherit;
      cursor: pointer;
    }}
    #orca-auth button:hover {{ background: #74c7ec; }}
    #orca-auth .status {{ margin-left: auto; opacity: 0.85; }}
    #orca-auth .ok {{ color: #a6e3a1; }}
    #orca-auth .err {{ color: #f38ba8; }}
  </style>
</head>
<body>
  <div id="orca-auth">
    <strong>orca:</strong>
    <input id="orca-u" placeholder="username" autocomplete="username" />
    <input id="orca-p" type="password" placeholder="password" autocomplete="current-password" />
    <button id="orca-signin" type="button">Sign in</button>
    <button id="orca-signout" type="button" style="display:none">Sign out</button>
    <span class="status" id="orca-status">checking session…</span>
  </div>
  <script>
    (function () {{
      const status = document.getElementById('orca-status');
      const u = document.getElementById('orca-u');
      const p = document.getElementById('orca-p');
      const signinBtn = document.getElementById('orca-signin');
      const signoutBtn = document.getElementById('orca-signout');
      const setStatus = (msg, cls) => {{
        status.textContent = msg;
        status.className = 'status ' + (cls || '');
      }};
      const setSignedIn = (signedIn) => {{
        u.style.display = signedIn ? 'none' : '';
        p.style.display = signedIn ? 'none' : '';
        signinBtn.style.display = signedIn ? 'none' : '';
        signoutBtn.style.display = signedIn ? '' : 'none';
      }};
      const checkMe = async () => {{
        try {{
          const r = await fetch('/api/auth/web/me', {{
            credentials: 'include',
            headers: {{ 'Accept': 'application/json' }},
          }});
          if (r.ok) {{
            const j = await r.json();
            setSignedIn(true);
            setStatus(`signed in as ${{j.username}} (${{j.role}})`, 'ok');
          }} else {{
            setSignedIn(false);
            setStatus('not signed in', 'err');
          }}
        }} catch (e) {{
          setSignedIn(false);
          setStatus(`probe error: ${{e}}`, 'err');
        }}
      }};
      document.getElementById('orca-signin').addEventListener('click', async () => {{
        const username = document.getElementById('orca-u').value.trim();
        const password = document.getElementById('orca-p').value;
        if (!username || !password) {{ setStatus('need username + password', 'err'); return; }}
        setStatus('signing in…');
        try {{
          const r = await fetch('/api/auth/web/signin', {{
            method: 'POST',
            credentials: 'include',
            headers: {{ 'Content-Type': 'application/json' }},
            body: JSON.stringify({{ username, password }}),
          }});
          if (r.ok) {{
            document.getElementById('orca-p').value = '';
            await checkMe();
          }} else {{
            const txt = await r.text();
            setStatus(`signin failed: ${{r.status}} ${{txt}}`, 'err');
          }}
        }} catch (e) {{ setStatus(`signin error: ${{e}}`, 'err'); }}
      }});
      document.getElementById('orca-signout').addEventListener('click', async () => {{
        try {{
          await fetch('/api/auth/web/signout', {{ method: 'POST', credentials: 'include' }});
        }} catch (_e) {{}}
        await checkMe();
      }});
      checkMe();
    }})();
  </script>
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

/// Single source of truth for all background runtime tasks. Called from
/// every serve path (run, run_daemon dev_spawn, run_daemon production) so
/// dev/stable can never silently diverge — adding a new background task
/// here arms it everywhere.
async fn spawn_all_runtime_tasks(pki_dir: &std::path::Path) {
    // Convert the on-disk db out of WAL to a rollback journal FIRST — before
    // the pool or any background task opens a connection. SQLCipher + WAL +
    // multiple in-process connections short-reads the shared wal-index and
    // fails every fresh open with 522; the conversion needs exclusive access,
    // so it must win uncontested here (see db::ensure_rollback_journal).
    if let Err(e) = db::ensure_rollback_journal() {
        tracing::warn!("rollback-journal conversion failed at startup: {e:#}");
    }
    // Initialize the process-wide DB connection pool BEFORE any task that
    // touches the DB spawns. Pays the SQLCipher KDF + page-cache allocation
    // once at startup instead of on every tool call.
    if let Err(e) = db::pool::DbPool::init_or_get() {
        tracing::warn!("db pool init failed at startup, falling back to per-call opens: {e:#}");
    }
    if let Err(e) = auth::loopback_token::install_at_startup() {
        tracing::warn!("loopback token install failed: {e:#}");
    }
    // One-shot capability probe. Populates `host_capabilities` so
    // topology collectors + provider tool surfaces can gate on
    // `is_available` and stop logging warn-every-tick for absent
    // runtimes. Operator-driven recheck via `system.capability.*`.
    if let Err(e) = system::capability::probe_all_capabilities().await {
        tracing::warn!("capability probe pass failed: {e:#}");
    }
    // Scan the persistent plugin install dir and load+gate every sideloaded
    // cdylib. Each plugin is gated independently; an incompatible one is logged
    // and skipped, never fatal. Synchronous (dlopen + abi_stable check) and
    // fast, so it runs inline before serving begins — loaded plugin tools are
    // then routable the moment the listener binds.
    let (loaded, failed) = system::plugin_manager::scan_and_load();
    if !loaded.is_empty() || !failed.is_empty() {
        tracing::info!(
            loaded = ?loaded,
            failed = ?failed,
            "plugin install-dir scan complete"
        );
    }
    // Replay the user's persisted web-route ownership choices now that every web
    // provider has registered. Contested paths otherwise default to the incumbent
    // (first registered); this promotes the provider the user selected.
    apply_persisted_web_owners();
    // Install the cdylib-plugin fallback into `dispatch` so loaded plugin tools
    // share the one REST/MCP/CLI dispatch entrypoint without dispatch having to
    // depend on plugin-loader (which would be a cycle). The host owns the wiring.
    dispatch::set_dynamic_dispatch(
        Box::new(plugin_loader::invoke_plugin),
        Box::new(|| {
            plugin_loader::loaded_tool_defs()
                .iter()
                .filter_map(|d| serde_json::to_value(d).ok())
                .collect()
        }),
    );
    system::system_info::spawn_refresher();
    pod::host_status_writer::spawn_local_writer();
    pod::host_status_writer::spawn_sync_puller();
    pod::host_status_sweep::spawn();
    system::maintenance::spawn_periodic();
    pod::host_status_replica::spawn_fleet_replicator();
    pod::update_state_probe::spawn_periodic();
    pod::system_detail_probe::spawn_periodic();
    spawn_pod_runtime(pki_dir).await;
    spawn_scheduler_runtime();
    tokio::spawn(system::commands::startup_update_check());
    if let Some(src) = system::dev::read_dev_source() {
        tokio::spawn(dev_source_auto_poll(src));
    }
}

/// Watches `~/.orca/dev-source` every 10 s; when a newer locally-built
/// binary appears, applies it and self-exits so the service manager
/// respawns into the new binary.
async fn dev_source_auto_poll(src: String) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    interval.tick().await; // skip immediate tick
    loop {
        interval.tick().await;
        match system::dev::check_for_update_dev(&src).await {
            Ok(Some(_)) => {
                tracing::info!("[dev] new build detected — applying and restarting");
                if let Err(e) = system::dev::apply_update_dev(&src).await {
                    tracing::warn!("[dev] apply failed: {e}");
                } else {
                    std::process::exit(0);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("[dev] update check: {e}"),
        }
    }
}

/// Best-effort startup of the pod-mesh runtime: mDNS responder + auto-offer
/// scheduler. Returns even if the bootstrap key can't be generated (e.g.
/// PKI dir unwritable) — pod features are simply unavailable for this run.
async fn spawn_pod_runtime(pki_dir: &std::path::Path) {
    // Detect mesh certs issued under the old `peer.<hostname>` CN
    // convention and reset them. The current convention is bare
    // `<machine_id_short>`; mixing produces duplicate pod_peers rows (one
    // keyed on the old CN via the listener stub, one on the new
    // machine_id_short CN via join-confirm).
    if let Err(e) = pod::reset_if_stale_mesh_identity(pki_dir) {
        tracing::warn!("[pod] stale-cert check failed: {e:#}");
    }

    match pod::mdns::build_advertisement(pki_dir.to_path_buf(), db::ports::mesh_port()) {
        Ok(ad) => {
            // Self-heal stale-self identity rows: a pod_discovery row whose
            // hostname matches ours but whose pubkey_fp differs is a previous
            // identity (key rotation, daemon reinstall, factory reset) that
            // would otherwise show up in the UI as "DEAD/STALE SELF IDENTITY"
            // every deploy. Evict on startup; mDNS will repopulate the
            // current-identity row within a few seconds.
            match db::open_default() {
                Ok(conn) => {
                    if let Err(e) = db::pod::evict_stale_self(&conn, &ad.hostname, &ad.pubkey_fp) {
                        tracing::warn!("[pod] stale-self eviction failed: {e:#}");
                    }
                }
                Err(e) => tracing::warn!("[pod] stale-self eviction: db open failed: {e:#}"),
            }
            match pod::mdns::Mdns::start(ad) {
                Ok(handle) => {
                    info!("[pod] mDNS responder + discoverer up");
                    // Park the handle in a process-static slot so the
                    // ServiceDaemon (and its browse task) live for the
                    // daemon lifetime. Dropping it tears down the
                    // responder + discoverer within ~1s; `republish` is
                    // also unreachable without a stable handle.
                    static MDNS: std::sync::OnceLock<pod::mdns::Mdns> = std::sync::OnceLock::new();
                    if MDNS.set(handle).is_err() {
                        tracing::warn!("[pod] mDNS handle already parked");
                    }
                }
                Err(e) => tracing::warn!("[pod] mDNS start failed: {e:#}"),
            }
        }
        Err(e) => tracing::warn!("[pod] cannot build mDNS advertisement: {e:#}"),
    }

    std::mem::drop(pod::scheduler::spawn());
    info!("[pod] auto-offer scheduler armed");

    // Mesh TCP+mTLS accept loop on `db::ports::mesh_port()` (default 12002).
    //
    // Always spawn — the BOOTSTRAP SNI path
    // (`utils::pki::POD_BOOTSTRAP_SAN`) is how unpaired hosts receive
    // incoming offers and accept pairing. Gating on `mesh_server_cert`
    // makes non-pod-members un-inviteable (mint can't dial 12002 →
    // pairing can never start). `build_acceptor` materializes the
    // bootstrap cert eagerly, and `HotReloadResolver` returns `None` for
    // `POD_SERVER_SAN` until the mesh server cert lands on disk — so
    // paired-peer handshakes are correctly refused on unpaired hosts
    // without blocking the bootstrap path.
    match pod::mesh_listener::spawn(pki_dir).await {
        Ok(handle) => {
            info!("[pod] mesh listener up on :{}", db::ports::mesh_port());
            std::mem::drop(handle);
        }
        Err(e) => tracing::warn!("[pod] mesh listener spawn failed: {e:#}"),
    }

    std::mem::drop(pod::cert_rotation::spawn());
    info!("[pod] cert-rotation scheduler armed (daily)");

    std::mem::drop(pod::roster_sync::spawn());
    info!("[pod] roster-sync armed (60s) — auto-fills pod_peers from any paired peer");

    if let Err(e) = db::replicate_engine::register(pod::transport::PodMeshTransport::new()) {
        tracing::warn!("[replicate] transport register failed: {e:#}");
    }
    let _ = db::replicate_engine::spawn();

    std::mem::drop(system::host_identity::spawn_refresh_task());
    info!("[host-addressing] refresh task armed (5m)");
}

/// Build the tool ctx and spawn the in-process cron scheduler. Nested tool
/// dispatch (e.g. `schedule.run` invoking another tool) goes through the
/// shared `dispatch::dispatch` free fn, which walks the inventory
/// directly. Best-effort: a config-load failure disables the scheduler but
/// does not abort the daemon.
fn spawn_scheduler_runtime() {
    match contract::config::Config::load() {
        Ok(cfg) => {
            let cfg = Arc::new(cfg);
            let ctx = Arc::new(crate::mcp::build_tool_ctx(cfg));
            std::mem::drop(system::scheduler::spawn(ctx));
            info!("[scheduler] in-process cron scheduler armed (60s tick)");
            std::mem::drop(system::storage_selfheal::spawn());
            info!(
                "[selfheal] autofs self-heal loop armed ({}s tick, confirm×{})",
                system::storage_selfheal::INTERVAL_SECS,
                system::storage_selfheal::CONFIRM_TICKS
            );
        }
        Err(e) => tracing::warn!("[scheduler] Config::load failed, scheduler disabled: {e}"),
    }
}

/// Walk the parent-process chain on Linux looking for `cargo-watch`. Catches
/// legacy cargo-watch instances on peers that pre-date the
/// `ORCA_DEV_PARENT_PID` env-var convention (the daemon's immediate parent
/// is usually `sh` from `cargo watch -x run -- daemon start`, so we have to
/// walk up). Returns false on non-Linux and on any /proc read error.
fn spawned_by_cargo_watch() -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
    #[cfg(target_os = "linux")]
    {
        fn ppid_and_comm(pid: u32) -> Option<(u32, String)> {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()?
                .trim()
                .to_owned();
            let ppid = status
                .lines()
                .find_map(|l| l.strip_prefix("PPid:"))
                .and_then(|v| v.trim().parse().ok())?;
            Some((ppid, comm))
        }
        let mut pid = std::process::id();
        for _ in 0..8 {
            match ppid_and_comm(pid) {
                Some((0, _)) | None => return false,
                Some((parent, _)) => {
                    let parent_comm = std::fs::read_to_string(format!("/proc/{parent}/comm"))
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    if parent_comm == "cargo-watch" {
                        return true;
                    }
                    pid = parent;
                }
            }
        }
        false
    }
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
    if let Some(path) = utils::path::which("orca") {
        return path;
    }
    std::env::current_exe()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

// Web UI — served by a registered `contract::web::WebProvider` (the `peacock`
// out-of-process plugin), not embedded in core. The fallback router dispatches
// each request to the provider whose route prefix is the longest match, with
// the owner of `/` as the catch-all. When no provider is registered (headless),
// the handler returns a plain 404. Independently, the `ui.enabled` DB setting
// gates the root (`/`) owner at runtime (default true, read once at startup).

static UI_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ui_enabled() -> bool {
    *UI_ENABLED.get_or_init(|| {
        // Resolve through `db::open_default()` so we honour the same task-
        // local / env-var / encrypted-default resolution path as every other
        // handler. Doing this lazily on first request (not in build_router)
        // avoids forcing the production encrypted-open codepath on tests
        // that supplied an unencrypted DB via task-local.
        let enabled = db::open_default()
            .ok()
            .and_then(|c| db::feature_flags::get(&c, "ui.enabled").ok().flatten())
            .unwrap_or(true);
        tracing::info!("ui.enabled = {enabled}");
        enabled
    })
}

/// Replay every persisted web-route ownership choice into the registry at boot.
/// Best-effort + logged: a choice referencing a provider that did not register
/// this run is skipped (the incumbent holds), never fatal.
fn apply_persisted_web_owners() {
    let Ok(conn) = db::open_default() else {
        return;
    };
    let Ok(rows) = db::settings::list_prefix(&conn, contract::web::WEB_OWNER_SETTING_PREFIX) else {
        return;
    };
    for (key, provider) in rows {
        let Some(path) = key.strip_prefix(contract::web::WEB_OWNER_SETTING_PREFIX) else {
            continue;
        };
        if let Err(e) = contract::web::set_owner(path, &provider) {
            tracing::warn!(%path, %provider, error = %e, "persisted web owner not applied (provider absent this run)");
        }
    }
}

/// Fallback handler in prod: route the request to the matching registered
/// [`contract::web::WebProvider`], applying SPA fallback and a short-TTL cache.
async fn static_handler(req: axum::extract::Request) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{Response, header};

    let uri_path = req.uri().path().to_string();

    let Some(provider) = contract::web::resolve(&uri_path) else {
        return Response::builder()
            .status(404)
            .header("content-type", "text/plain")
            .body(Body::from(
                "no web UI plugin registered — install a web provider (e.g. peacock)",
            ))
            .expect("hardcoded response is valid");
    };

    // The `ui.enabled` gate applies only to the root (`/`) owner — the SPA.
    // Non-root asset/viewer providers are not toggled by it.
    if provider.route().prefix == "/" && !ui_enabled() {
        return Response::builder()
            .status(404)
            .header("content-type", "text/plain")
            .body(Body::from(
                "web UI disabled — set settings.ui.enabled = 'true' and restart",
            ))
            .expect("hardcoded response is valid");
    }

    let method = req.method().as_str().to_string();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let body_bytes = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap_or_default();

    // Cache only bodyless GETs (assets / SPA shell); anything with a request
    // body or a non-GET method bypasses the cache.
    let cacheable = method == "GET" && body_bytes.is_empty();
    let cache_key = format!("{}:{method} {uri_path}", provider.name());

    let render = |wr: contract::web::WebResponse| -> axum::response::Response {
        let mut builder = Response::builder().status(wr.status);
        let mut has_ct = false;
        for (k, v) in &wr.headers {
            if k.eq_ignore_ascii_case(header::CONTENT_TYPE.as_str()) {
                has_ct = true;
            }
            builder = builder.header(k, v);
        }
        if !has_ct {
            builder = builder.header(header::CONTENT_TYPE, "application/octet-stream");
        }
        let body = utils::encoding::base64_decode(&wr.body_b64).unwrap_or_default();
        builder
            .body(Body::from(body))
            .unwrap_or_else(|_| Response::builder().status(502).body(Body::empty()).unwrap())
    };

    if cacheable
        && let Some(hit) = db::cache::WEB_RESPONSE.get(&cache_key)
        && let Ok(wr) = serde_json::from_str::<contract::web::WebResponse>(&hit.response_json)
    {
        return render(wr);
    }

    let request = contract::web::WebRequest {
        path: uri_path.clone(),
        method,
        headers,
        body_b64: if body_bytes.is_empty() {
            String::new()
        } else {
            utils::encoding::base64_encode(&body_bytes)
        },
    };

    let mut resp = match provider.render(request.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(provider = %provider.name(), error = %e, "web render failed");
            return Response::builder()
                .status(502)
                .body(Body::from("web provider render failed"))
                .expect("502 response is valid");
        }
    };

    // SPA fallback: a bare 404 (no body) on a fallback-enabled provider is
    // retried as its index.html so client-side routing can resolve the path.
    if resp.status == 404
        && resp.body_b64.is_empty()
        && provider.route().spa_fallback
        && uri_path != "/index.html"
    {
        let index_req = contract::web::WebRequest {
            path: "/index.html".to_string(),
            ..request
        };
        if let Ok(idx) = provider.render(index_req).await {
            resp = idx;
        }
    }

    if cacheable
        && resp.status == 200
        && let Ok(json) = serde_json::to_string(&resp)
    {
        db::cache::WEB_RESPONSE.insert(
            cache_key,
            db::cache::WebResponseEntry {
                response_json: json,
            },
        );
    }

    render(resp)
}

// ── Dev proxy ─────────────────────────────────────────────────────────────────
// In dev mode, Rust owns port 12000 and proxies non-API requests to Vite at
// :12001. This means the browser always uses one port for both API and UI,
// matching the prod layout exactly.

const VITE_ORIGIN: &str = "http://127.0.0.1:12001";
const VITE_WS_ORIGIN: &str = "ws://127.0.0.1:12001";
// Storybook dev server. Launched by scripts/dev.sh alongside Vite so the
// browser sees one unified origin: /storybook/* is forwarded here and the
// rest of the UI continues to fall through to Vite.
const STORYBOOK_ORIGIN: &str = "http://127.0.0.1:12002";
const STORYBOOK_WS_ORIGIN: &str = "ws://127.0.0.1:12002";

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
    // Route-driven: forward to the matching web provider's registered
    // `dev_upstream` (its `npm run dev` Vite server) rather than a hardcoded
    // `projects/frontend` origin. Falls back to the legacy `VITE_ORIGIN` const
    // only if no provider declares a dev upstream (keeps bare-repo dev working).
    let (http_origin, ws_origin) = contract::web::resolve(req.uri().path())
        .and_then(|p| p.route().dev_upstream.clone())
        .map(|http| {
            let ws = http
                .replacen("http://", "ws://", 1)
                .replacen("https://", "wss://", 1);
            (http, ws)
        })
        .unwrap_or_else(|| (VITE_ORIGIN.to_string(), VITE_WS_ORIGIN.to_string()));
    proxy_to(req, http_origin, ws_origin).await
}

async fn storybook_proxy_handler(req: axum::extract::Request) -> axum::response::Response {
    proxy_to(
        req,
        STORYBOOK_ORIGIN.to_string(),
        STORYBOOK_WS_ORIGIN.to_string(),
    )
    .await
}

async fn proxy_to(
    req: axum::extract::Request,
    http_origin: String,
    ws_origin: String,
) -> axum::response::Response {
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
        return match WebSocketUpgrade::from_request(req, &()).await {
            Ok(ws) => ws.on_upgrade(move |sock| proxy_ws(sock, path, ws_origin)),
            Err(e) => e.into_response(),
        };
    }

    proxy_http(req, http_origin).await
}

async fn proxy_http(req: axum::extract::Request, origin: String) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::Response;

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let url = format!("{origin}{path_and_query}");

    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    // Process-cached client so the dev-proxy doesn't churn a new connection
    // pool per request.
    static DEV_PROXY_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = DEV_PROXY_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("dev proxy reqwest client")
    });
    let mut rb = client.request(method, &url);

    for (k, v) in req.headers() {
        if is_hop_by_hop(k.as_str()) || k == axum::http::header::HOST {
            continue;
        }
        rb = rb.header(k.as_str(), v);
    }

    // Match DefaultBodyLimit (4 MiB). Larger payloads would buffer entirely in
    // RAM here, defeating the global cap.
    let body = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
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
                "orca: dev upstream unreachable — is the dev server running?",
            ))
            .expect("502 response is valid"),
    }
}

async fn proxy_ws(mut browser: axum::extract::ws::WebSocket, path: String, ws_origin: String) {
    use axum::extract::ws::Message as BMsg;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message as VMsg};

    let url = format!("{ws_origin}{path}");
    let (mut vite, _) = match connect_async(&url).await {
        Ok(v) => v,
        Err(_) => {
            _ = browser.close().await;
            return;
        }
    };

    loop {
        tokio::select! {
            msg = browser.recv() => match msg {
                Some(Ok(BMsg::Text(t)))   => { _ = vite.send(VMsg::Text(t.as_str().into())).await; }
                Some(Ok(BMsg::Binary(b))) => { _ = vite.send(VMsg::Binary(b.to_vec().into())).await; }
                Some(Ok(BMsg::Ping(p)))   => { _ = vite.send(VMsg::Ping(p.to_vec().into())).await; }
                Some(Ok(BMsg::Pong(p)))   => { _ = vite.send(VMsg::Pong(p.to_vec().into())).await; }
                _ => break,
            },
            msg = vite.next() => match msg {
                Some(Ok(VMsg::Text(t)))   => { _ = browser.send(BMsg::Text(t.as_str().into())).await; }
                Some(Ok(VMsg::Binary(b))) => { _ = browser.send(BMsg::Binary(b.to_vec().into())).await; }
                Some(Ok(VMsg::Ping(p)))   => { _ = browser.send(BMsg::Ping(p.to_vec().into())).await; }
                Some(Ok(VMsg::Pong(p)))   => { _ = browser.send(BMsg::Pong(p.to_vec().into())).await; }
                _ => break,
            },
        }
    }

    _ = browser.close().await;
}

/// Build the axum `Router` — exposed so integration tests can call it directly.
pub fn build_router(dev: bool, db_path: std::path::PathBuf) -> Router {
    use std::sync::Arc;

    // Ensures reqwest (rustls-no-provider) has a crypto provider; idempotent.
    ::model::ensure_crypto_provider();

    // Plumb the dev flag to auth_routes so session cookies use SameSite=None
    // in dev (cross-port) and SameSite=Strict in prod (same-origin).
    auth_routes::set_dev_mode(dev);

    // Always mirror the requesting origin so cookie-bearing credentialed
    // fetches work in all access patterns: Vite dev server (:12001 → :12000
    // cross-port), direct embedded UI (same-origin), and remote browser
    // access from another machine on the LAN.
    let cors = {
        let _ = dev; // suppress unused warning if cfg changes
        CorsLayer::new()
            .allow_origin(AllowOrigin::mirror_request())
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                HeaderName::from_static("content-type"),
                HeaderName::from_static("authorization"),
                HeaderName::from_static("x-correlation-id"),
            ])
            .allow_credentials(true)
    };

    let mcp_pool = Arc::new(::mcp::client::McpPool::new_with_db(db_path));

    let (api, spec) = openapi::openapi_router().split_for_parts();
    // Stash the assembled spec so the spec-serving handlers can read it.
    openapi::install_spec(spec);
    // Write orca's own spec to disk so it lives alongside scanner-generated specs.
    write_orca_spec_to_disk();

    let api = api
        // Spec endpoints — registered after split so they are not themselves
        // documented in the spec (would be circular and noisy).
        .route("/api/health", get(ping_handler))
        .route("/api/openapi.json", get(openapi::openapi_handler))
        .route(
            "/api/openapi/public.json",
            get(openapi::openapi_public_handler),
        )
        // Live managed-unit catalog for CLI runtime service discovery.
        .route("/api/catalog", get(openapi::unit_catalog_handler))
        // Scalar API reference viewer — served by Rust so it works in the
        // prerendered static build (SvelteKit SSR routes don't survive embedding).
        // One unified spec: per-operation `x-codeSamples` render REST / CLI /
        // MCP invocation forms as tabs on the same page.
        .route("/scalar", get(scalar_handler))
        // Open probe: lets the browser TokenGate decide which UI to show
        // (one-click bootstrap vs. paste an existing token).
        .route("/api/auth/bootstrap", get(bootstrap_status_handler))
        // Web-UI account auth routes (signup_status / signup / signin /
        // signout / me) are wired via `openapi_router()` so they appear in
        // the emitted OpenAPI spec for the hey-api codegen pipeline.
        .with_state(mcp_pool);

    // Mount the OrcaTool registry under /api/v1. Same registry as MCP stdio
    // and CLI — one trait impl, three live surfaces (REST + MCP + CLI).
    let api = match contract::config::Config::load() {
        Ok(cfg) => {
            // Reuse the same registry + service-trait setup that the CLI and
            // MCP-stdio surfaces use, otherwise tools that look up services on
            // ToolCtx (lifecycle, profile, pki, etc.) return 500.
            let cfg = Arc::new(cfg);
            let ctx = Arc::new(crate::mcp::build_tool_ctx(cfg));
            // Share the ctx with the pod relay so peer-relayed tool calls
            // dispatch in-process instead of looping back over HTTPS with
            // the admin token (M4 in the v1 hardening punch list). Dispatch
            // walks the inventory directly — no registry to ship.
            pod::dispatcher::install(ctx.clone());
            api.nest("/api/v1", dispatch::axum_router(ctx))
        }
        Err(e) => {
            tracing::warn!("Config::load failed, /api/v1/* disabled: {e}");
            api
        }
    };

    // Layers apply AFTER all nesting so /api/v1/* inherits auth + logging.
    // Outermost to innermost (last added = outermost): CORS → log_requests →
    // require_auth → handler. Logging sits OUTSIDE auth so 401s are still
    // logged — otherwise rejected requests vanish silently from the log.
    let api = api
        // Cap request bodies so a malicious or runaway client can't OOM the
        // daemon by streaming an unbounded payload at a `Bytes`/raw-body
        // extractor. Tool payloads are JSON and fit comfortably under this.
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(axum::middleware::from_fn(middleware::require_tool_role))
        .layer(axum::middleware::from_fn(middleware::require_auth))
        .layer(axum::middleware::from_fn(middleware::log_requests))
        .layer(cors);

    if dev {
        // Storybook lives at <baseUrl>/storybook in dev — see .storybook/main.ts
        // (viteFinal sets base = '/storybook/') and scripts/dev.sh which launches
        // it on :12002. The Rust proxy forwards both HTTP and HMR WebSockets.
        api.route("/storybook", any(storybook_proxy_handler))
            .route("/storybook/{*path}", any(storybook_proxy_handler))
            .fallback(dev_proxy_handler)
    } else {
        api.fallback(static_handler)
    }
}

/// Write orca's generated OpenAPI spec to ~/.orca/specs/orca.json so it
/// lives alongside scanner-generated specs and can be compared to them.
fn write_orca_spec_to_disk() {
    let dir = db::openapi_specs_registry::specs_dir();
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── M1 guard ──────────────────────────────────────────────────────────────

    #[test]
    fn dev_multi_user_guard_allows_zero_users() {
        dev_multi_user_guard(0).unwrap();
    }

    #[test]
    fn dev_multi_user_guard_allows_one_user() {
        dev_multi_user_guard(1).unwrap();
    }

    #[test]
    fn dev_multi_user_guard_refuses_two_or_more_users() {
        let err = dev_multi_user_guard(2).unwrap_err();
        assert!(err.to_string().contains("--dev refused"), "got: {}", err);
    }

    // ── is_hop_by_hop ─────────────────────────────────────────────────────────

    #[test]
    fn is_hop_by_hop_matches_known_headers() {
        for h in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "te",
            "trailer",
            "upgrade",
        ] {
            assert!(is_hop_by_hop(h), "expected hop-by-hop: {h}");
        }
    }

    #[test]
    fn is_hop_by_hop_does_not_match_end_to_end_headers() {
        for h in ["content-type", "authorization", "accept", "x-request-id"] {
            assert!(!is_hop_by_hop(h), "unexpected hop-by-hop: {h}");
        }
    }

    // ── pid_alive ─────────────────────────────────────────────────────────────

    #[test]
    fn pid_alive_is_true_for_current_process() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_is_false_for_impossible_pid() {
        // PID 4_000_000 is far beyond any real process ID on macOS/Linux.
        assert!(!pid_alive(4_000_000));
    }

    // ── resolve_daemon_binary ─────────────────────────────────────────────────

    #[test]
    fn resolve_daemon_binary_returns_nonempty_string() {
        let path = resolve_daemon_binary();
        assert!(!path.is_empty());
    }
}
