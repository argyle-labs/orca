use ::model::{ClaudeBackend, Message, ModelBackend, stdout_sink};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use contract::config::Config;
use conversation::log_cmd::{LogAction, cmd_log};
use conversation::sessions::context::ProjectContext;
use conversation::sessions::session::Session;
use dev::dev_serve as dev_serve_cmd;
use orca::mcp;
use orca::serve;
use orca::serve::openapi::orca_spec_json;
use system::hook::{self as hook_cmd, HookAction};

#[derive(Parser)]
#[command(name = "orca", about = "Context-first AI agent orchestrator", version)]
struct Cli {
    /// Project context to load. Omit for general session.
    #[arg(value_name = "PROJECT")]
    project: Option<String>,

    /// Use classic readline mode instead of the split-pane TUI.
    #[arg(long)]
    classic: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ask Claude directly (escalation, non-interactive)
    Escalate {
        question: String,
        #[arg(long)]
        project: Option<String>,
    },

    /// Run Bear audit on a project (dependency vulnerabilities, code review)
    Audit {
        /// Path to project directory (default: current directory)
        #[arg(default_value = ".")]
        path: String,
    },

    /// Search and manage session logs
    Log {
        #[command(subcommand)]
        action: LogAction,
    },

    /// One-shot: send prompt to an agent and print response
    Run {
        #[arg(short = 'a', long, default_value = "wolf")]
        agent: String,
        prompt: String,
    },

    /// Start MCP stdio server — exposes orca tools to Claude Code
    McpServe,

    /// Start the orca web server (docs + services UI)
    Serve {
        /// Dev mode: spawn Vite dev server for hot reload
        #[arg(long)]
        dev: bool,
        /// HTTP port to bind. Unset ⇒ resolved per-instance via
        /// `db::ports::http_port()` (env `ORCA_HTTP_PORT` > persisted DB port >
        /// const `APP_REST_HTTP_PORT`). An explicit `--port` overrides all.
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Run as daemon with cooperative port handoff (SIGUSR1 park / SIGUSR2 reclaim).
    /// `system.daemon.{status,stop,park,reclaim,install,uninstall}` are tools.
    Daemon {
        /// HTTP port to bind. Unset ⇒ resolved per-instance via
        /// `db::ports::http_port()` (env > persisted DB port > const).
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Start dev server, superseding any running daemon on the port.
    /// Parks the stable daemon, runs dev mode, reclaims on exit.
    Dev {
        /// HTTP port to bind. Unset ⇒ resolved per-instance via
        /// `db::ports::http_port()` (env > persisted DB port > const).
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Serve the locally-built linux binary for fleet hot-reload.
    ///
    /// Run this on the dev machine after `cargo build --release --target x86_64-unknown-linux-gnu`.
    /// On each peer: `orca update --source http://<dev-ip>:12009`
    /// The daemon auto-polls and restarts when a new build lands.
    DevServe {
        /// Path to the binary to serve (default: target/x86_64-unknown-linux-gnu/release/orca).
        #[arg(long, value_name = "PATH")]
        binary: Option<std::path::PathBuf>,
        /// Port to listen on (default: 12009).
        #[arg(long, default_value_t = 12009)]
        port: u16,
    },

    /// Pod / mesh networking — bootstrap, ping, peer management.
    Pod {
        #[command(subcommand)]
        action: PodAction,
    },

    /// Claude Code hook handlers (session-start, bash-guard, pii-scan, etc.)
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Emit orca's own OpenAPI 3 spec to stdout as raw JSON. Used by the
    /// frontend codegen pipeline (`hey-api` reads this to generate the
    /// typed TS client). Unlike `orca spec dump` (OrcaTool wrapper), this
    /// prints the spec object directly without a `{spec: "..."}` envelope.
    Openapi {
        #[command(subcommand)]
        action: OpenapiAction,
    },

    /// Local-only administrative commands. Never exposed over REST/MCP — these
    /// require shell access to a paired host with DB access ("secure system").
    Admin {
        #[command(subcommand)]
        action: AdminAction,
    },

    /// Passthrough for `OrcaOp`-migrated domains — dispatched via inventory.
    /// Captures any first arg not matching a derive variant above; the
    /// `dispatch::cli` inventory routes it to the right tool.
    #[command(external_subcommand)]
    Op(Vec<String>),
}

#[derive(Subcommand)]
enum PodAction {
    /// Founder bootstrap. Creates the mesh CA + this host's pod cert. Idempotent.
    Init,
    /// Send a `pod/ping` to a peer over mTLS (SNI=pod.orca.local) and print the result.
    Ping { host: String },
    /// Show orcas seen on the network (mDNS-discovered).
    Discover,
    /// Show pending pod-membership offers awaiting `pod accept`.
    Pending,
    /// Accept an inbound offer by pairing code (printed on the inviter's CLI).
    Accept { code: String },
    /// Manual fallback when mDNS doesn't see the inviter — point at a
    /// specific addr `host[:port]`. Alias for `pod join`.
    Connect { addr: String },
    /// Joiner-initiated handshake. Dials the inviter over the bootstrap
    /// channel (TOFU first contact, then signed-echo fp check) and lands a
    /// pending inbound offer ready for `pod accept`.
    Join { addr: String },
    /// Manually push an offer to a known address (inviter side, when
    /// mDNS doesn't see the joiner).
    Offer { addr: String },
    /// One-shot inviter flow: push an offer to `addr`, print the pairing code,
    /// and block until the joiner accepts (or the offer expires). Wraps
    /// `pod offer` + the wait the operator would otherwise do manually.
    Pair { addr: String },
    /// List known peers and their trust state.
    List,
    /// Mark a peer as locally trusted (or untrust). Triggers CA-key
    /// replication when both sides have flagged each other secure.
    Trust {
        /// Peer ID (e.g. `peer.host-g`).
        peer_id: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Toggle whether THIS host stores secrets locally. Default off on
    /// fresh joiners; `pod init` flips it on automatically.
    SelfSecure {
        #[arg(value_parser = ["on", "off", "show"], default_value = "show")]
        state: String,
    },
    /// Show days-remaining + rotation status for every cert on this host.
    CertStatus,
    /// Rotate the mesh CA. Both old (`previous`) and new (`current`) CAs are
    /// trusted during the overlap window; existing peer certs stay valid
    /// until they auto-rotate or the overlap expires.
    CaRotate {
        #[arg(long, default_value_t = 14)]
        overlap_days: i64,
    },
    /// Leave the pod. Notifies peers, wipes mesh PKI + pod tables.
    /// Use `--wipe-secrets` to also truncate the secrets table; `--wipe-all`
    /// for a near-fresh-install state (also wipes plugin_data, oauth tokens,
    /// profile credentials). Bootstrap identity (host pubkey) is preserved.
    Leave {
        #[arg(long)]
        wipe_secrets: bool,
        #[arg(long)]
        wipe_all: bool,
    },
}

#[derive(Subcommand)]
enum OpenapiAction {
    /// Print the live OpenAPI 3 JSON spec to stdout (no envelope, no server boot).
    Emit,
}

#[derive(Subcommand)]
enum AdminAction {
    /// Reset a user's password. Reads the new password from stdin (no plaintext
    /// on argv). Requires shell access to this host's `orca` user — there is
    /// deliberately no REST/MCP/peer surface for this command.
    ResetPassword {
        /// Username (case-insensitive). Must already exist in `users`.
        username: String,
        /// Revoke every active session for the user after the password change.
        /// Default true.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        revoke_sessions: bool,
    },
    /// Privileged autofs applier: reads a JSON `PrivilegedOp` from stdin and
    /// executes it as root (write validated config files + restart autofs, or
    /// force-unmount wedged mounts). Invoked by the daemon via `sudo -n` — the
    /// one privileged surface for storage. Never exposed over REST/MCP/peer.
    StorageApply,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Unified logging: JSON-line output, EnvFilter from `ORCA_LOG`, a
    // ScrubWriter that redacts well-known sensitive patterns (PVE API
    // tokens, Authorization headers, JSON secret fields) before they
    // reach stderr or the tee'd dev log. See `plugin_toolkit::logging`.
    plugin_toolkit::logging::init(plugin_toolkit::logging::LogInit {
        env_var: "ORCA_LOG",
        default_filter: "warn,orca=info,tower_http=warn,axum=warn,mdns_sd=warn,mdns=warn",
        tee_path: Some("/tmp/orca-dev.log"),
    })?;

    // Install the CLI→daemon HTTP transport. `dispatch` routes the local-daemon
    // round-trip through the `DaemonClient` trait and links no HTTP client of
    // its own; the reqwest impl lives here, in the binary. This keeps plugins
    // (which link `dispatch` only for the tool surface) free of reqwest/rustls.
    orca::daemon_client::HttpDaemonClient::install();

    // ntfy extracted to ~/code/ntfy (argyle-labs/ntfy) — its notification
    // backends now register through the plugin-loader's `notifications`-domain
    // proxy seam (one `NotifyProxy` per enabled endpoint, advertised by the
    // cdylib's `backends()`), like jellyfin/plex/nfs load via the loader. No
    // static `ntfy::bootstrap()` call site remains.

    // smb extracted to ~/code/argyle-labs/smb (argyle-labs/smb) — its storage
    // backend now registers through the plugin-loader's `storage`-domain proxy
    // seam (the cdylib's `backends()` advertises one network-share backend),
    // exactly like the ntfy extraction above. No static `smb::bootstrap()`
    // call site remains.

    // Short-circuit OrcaOp ops *before* clap parse: the derive `Cli` has a
    // positional `project: Option<String>` that would otherwise swallow the
    // domain name (`orca engine list` → project=engine, command="list").
    //
    // Require both domain AND verb to be registered (or `--help`) — that way
    // legacy subcommands like `orca spec dump` still fall through to the
    // derive parser when their verb isn't a migrated OrcaOp.
    //
    // Try progressively longer domain prefixes so dotted sub-domains (e.g.
    // "system.dev") work as space-separated CLI args (`orca system dev update`
    // rather than requiring `orca system.dev update`).
    {
        let argv: Vec<String> = std::env::args().collect();
        if argv.len() >= 2 {
            let rest_args = &argv[1..];
            let matched = (1..=rest_args.len()).any(|depth| {
                let dom = rest_args[..depth].join(".");
                let verb_opt = rest_args.get(depth);
                let is_domain_help =
                    matches!(verb_opt.map(String::as_str), Some("--help") | Some("-h"));
                dispatch::cli::ops().any(|o| {
                    o.domain == dom
                        && (is_domain_help
                            || verb_opt.is_none()
                            || verb_opt.is_some_and(|v| o.verb == v))
                })
            });
            if matched {
                let config = Config::load()?;
                // OrcaTool-routed CLI commands bypass the legacy main()
                // path's init; do it here so any tool that touches
                // host_identity (e.g. pod.offer → push_offer) is safe.
                system::host_identity::init(&config.app_dir)?;
                let rest = rest_args.to_vec();
                return dispatch_op(rest, config).await;
            }
        }
    }

    let cli = Cli::parse();

    // Dispatch hook commands before Config::load() — hooks run in a subprocess context
    // where Keychain access (called inside Config::load) can hang and trigger a SIGKILL timeout.
    // Hook implementations are lightweight (regex, stdin, filesystem) and don't need Config.
    if let Some(Command::Hook { action }) = cli.command {
        return hook_cmd::cmd_hook(action);
    }

    let mut config = Config::load()?;
    // Capture hostname + load/generate machine_id once at startup so all
    // downstream code (mDNS, pod scheduler, cert rotation) sees a stable
    // identity regardless of OS hostname churn.
    system::host_identity::init(&config.app_dir)?;
    // Run TOML → DB migrations and auto-registration of detected runtimes.
    db::startup::init(&config);
    // Load API key from encrypted DB when not set via environment variable.
    if config.anthropic_api_key.is_none() {
        config.anthropic_api_key = db::startup::load_api_key(&config);
    }
    // Ensure a default profile exists for the implicit local user. v1 is
    // single-user; this becomes per-real-user once auth lands. Failures are
    // logged but non-fatal so commands that don't need a profile (e.g. setup
    // flows) still work.
    if let Err(e) = bootstrap_default_profile(&config) {
        tracing::warn!("profile bootstrap failed: {e}");
    }

    match cli.command {
        Some(Command::Escalate { question, project }) => {
            escalate(&config, &question, project.as_deref()).await
        }
        Some(Command::Log { action }) => cmd_log(&config, action),
        Some(Command::Audit { path }) => {
            let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.into());
            let prompt = format!(
                "Run a full audit on the project at {}. \
                 Check for dependency vulnerabilities (cargo audit, npm audit if applicable), \
                 review code for security issues, and check for cleanup opportunities \
                 (orphaned files, broken symlinks, dead code). \
                 Present findings as a prioritized list.",
                abs.display()
            );
            run_one_shot(&config, "bear", &prompt).await
        }
        Some(Command::Run { agent, prompt }) => run_one_shot(&config, &agent, &prompt).await,
        Some(Command::McpServe) => mcp::serve(&config).await,
        // Unset `--port` resolves per-instance (env > persisted DB port > const).
        Some(Command::Serve { dev, port }) => {
            let port = port.unwrap_or_else(db::ports::http_port);
            serve::run(dev, port, config.db_path.clone()).await
        }
        Some(Command::Daemon { port }) => {
            let port = port.unwrap_or_else(db::ports::http_port);
            serve::run_daemon(port, config.db_path.clone()).await
        }
        Some(Command::Dev { port }) => {
            let port = port.unwrap_or_else(db::ports::http_port);
            cmd_dev(port, &config).await
        }
        Some(Command::Hook { action }) => hook_cmd::cmd_hook(action),
        Some(Command::Admin { action }) => cmd_admin(action).await,
        Some(Command::Op(argv)) => dispatch_op(argv, config).await,
        Some(Command::DevServe { binary, port }) => {
            dev_serve_cmd::cmd_dev_serve(binary.as_deref(), port).await
        }
        Some(Command::Pod { action }) => match action {
            PodAction::Init => {
                let pki = pod::pki_dir();
                // CN = stable machine_id (display hostname is held separately).
                let host = system::host_identity::machine_id_short().to_string();
                utils::pki::init_mesh_ca(&pki, &host)?;
                // Ensure the bootstrap identity (Ed25519 key + self-signed
                // cert) is present from the moment this host is poddable.
                utils::pki::load_or_init_bootstrap_cert(&pki)?;
                let conn = db::open_default()?;
                db::pod::set_self_secure(&conn, true)?;
                let pod_id = utils::id::new_short();
                db::pod::set_pod_id(&conn, &pod_id)?;
                println!("✓ mesh CA initialized at {}", pki.join("mesh").display());
                println!("  pod id: {pod_id}");
                println!(
                    "  founder peer id: {host}  (machine_id; display: {})",
                    system::host_identity::hostname()
                );
                println!("  self_secure: true (secrets storage enabled)");
                println!(
                    "  next: start the daemon. Auto-offers will flow to any \
                     unclaimed orca on the LAN; user accepts on the joiner with \
                     `orca pod accept <code>` (the code is printed in the daemon log here)."
                );
                Ok(())
            }
            PodAction::Ping { host } => {
                let result = pod::ping(&host).await?;
                println!("✓ {host} responded:");
                println!("  peer_id: {}", result.peer_id);
                println!("  hostname: {}", result.hostname);
                println!("  version: {}", result.version);
                Ok(())
            }
            PodAction::Discover => pod::cli::cmd_pod_discover(),
            PodAction::Pending => pod::cli::cmd_pod_pending(),
            PodAction::Accept { code } => pod::cli::cmd_pod_accept(&code).await,
            PodAction::Connect { addr } => pod::cli::cmd_pod_connect(&addr).await,
            PodAction::Join { addr } => pod::cli::cmd_pod_join(&addr).await,
            PodAction::Offer { addr } => pod::cli::cmd_pod_offer(&addr).await,
            PodAction::Pair { addr } => pod::cli::cmd_pod_pair(&addr).await,
            PodAction::List => pod::cli::cmd_pod_list(),
            PodAction::Trust { peer_id, state } => {
                pod::cli::cmd_pod_trust(&peer_id, state == "on").await
            }
            PodAction::SelfSecure { state } => {
                use pod::cli::SelfSecureAction;
                let action = match state.as_str() {
                    "on" => SelfSecureAction::On,
                    "off" => SelfSecureAction::Off,
                    _ => SelfSecureAction::Show,
                };
                pod::cli::cmd_pod_self_secure(action)
            }
            PodAction::CertStatus => pod::cli::cmd_pod_cert_status(),
            PodAction::CaRotate { overlap_days } => pod::cli::cmd_pod_ca_rotate(overlap_days).await,
            PodAction::Leave {
                wipe_secrets,
                wipe_all,
            } => pod::cli::cmd_pod_leave(wipe_secrets, wipe_all).await,
        },
        Some(Command::Openapi { action }) => match action {
            OpenapiAction::Emit => {
                let spec = orca_spec_json();
                println!("{}", serde_json::to_string_pretty(&spec)?);
                Ok(())
            }
        },
        None => {
            let explicit = cli.project.as_deref().unwrap_or("");
            let project = if explicit.is_empty() {
                detect_project_from_cwd(&config).unwrap_or_default()
            } else {
                explicit.to_string()
            };
            let ctx = if project.is_empty() {
                ProjectContext::default()
            } else {
                ProjectContext::resolve(&project, &config)?
            };
            let mut session = Session::new(config, ctx).await?;
            if cli.classic {
                session.run().await
            } else {
                session.run_tui().await
            }
        }
    }
}

/// Ensure the implicit local user has a `default` profile on first run.
/// Idempotent on subsequent invocations.
fn bootstrap_default_profile(config: &Config) -> Result<()> {
    let conn = db::open(&config.db_path)?;
    let mgr = namespace::NamespaceManager::from_config(config);
    let p = mgr.ensure_default_for(&conn, contract::config::LOCAL_USER)?;
    tracing::trace!(profile_id = %p.id, "active profile resolved");
    Ok(())
}

async fn escalate(config: &Config, question: &str, project: Option<&str>) -> Result<()> {
    let api_key = config
        .anthropic_api_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no API key — run `orca login`"))?;

    let system = match project {
        Some(p) => {
            let ctx = ProjectContext::resolve(p, config)?;
            ctx.build_system_prompt(config)
        }
        None => String::new(),
    };

    let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
    let messages = vec![Message::user(question)];
    let cancel = tokio_util::sync::CancellationToken::new();
    let output = stdout_sink();
    claude
        .chat(&messages, &[], &system, cancel, &output)
        .await?;
    Ok(())
}

/// One-shot: load the named agent's system prompt, send prompt, print response, exit.
async fn run_one_shot(config: &Config, agent: &str, prompt: &str) -> Result<()> {
    let ctx = ProjectContext::default();
    let mut session = Session::new(config.clone(), ctx).await?;
    session.set_agent(agent);
    session.one_shot(prompt.to_string()).await
}

/// Park the stable daemon (if running), start dev server, reclaim on exit.
async fn cmd_dev(port: u16, config: &Config) -> Result<()> {
    use std::process::Command;
    use utils::state::DaemonMode;

    // The Vite dev server is no longer spawned here — the frontend lives in the
    // out-of-process `peacock` plugin, which owns its own `npm run dev`. `orca
    // dev` starts the daemon in dev mode (route-driven dev proxy forwards `/` to
    // whichever web provider declared a `dev_upstream`); run peacock's dev
    // server alongside from the peacock repo.

    // Park daemon if it's running
    let (daemon_pid, daemon_binary) = match utils::state::read()? {
        Some(s) if s.mode == DaemonMode::Daemon => {
            // Capture binary now — state file may be gone by the time we need it
            let binary = s.binary.clone();
            let pid = s.daemon_pid;
            Command::new("kill")
                .args(["-USR1", &pid.to_string()])
                .status()?;
            if let Err(e) = utils::state::wait_for_mode(DaemonMode::Parked, 5).await {
                // Parking timed out — reclaim immediately so daemon isn't stuck parked
                _ = Command::new("kill")
                    .args(["-USR2", &pid.to_string()])
                    .status();
                return Err(e.context("daemon did not park in time; reclaim sent"));
            }
            println!("[orca] daemon parked — dev server taking port {port}");
            (Some(pid), Some(binary))
        }
        _ => (None, None),
    };

    // Mark ourselves as the active dev process
    if let Some(mut s) = utils::state::read()? {
        s.mode = DaemonMode::Dev;
        s.active_pid = std::process::id();
        _ = utils::state::write(&s);
    }

    // Run dev server (Ctrl-C will exit)
    let result = serve::run(true, port, config.db_path.clone()).await;

    // Reclaim: read current state (daemon may have been restarted by launchd with a new PID)
    if daemon_pid.is_some() {
        let current_pid = utils::state::read()
            .ok()
            .flatten()
            .map(|s| s.daemon_pid)
            .or(daemon_pid);

        let reclaimed = current_pid
            .and_then(|pid| {
                Command::new("kill")
                    .args(["-USR2", &pid.to_string()])
                    .status()
                    .ok()
                    .filter(|s| s.success())
                    .map(|_| pid)
            })
            .is_some();

        if reclaimed {
            println!("[orca] daemon reclaimed port {port}");
        } else {
            // Daemon is not alive and was not restarted by launchd — spawn fresh
            let binary = utils::state::read()
                .ok()
                .flatten()
                .map(|s| s.binary)
                .or(daemon_binary);
            if let Some(bin) = binary {
                println!("[orca] daemon gone — respawning {bin}");
                _ = Command::new(&bin)
                    .args(["daemon", "--port", &port.to_string()])
                    .spawn();
            }
        }
    }

    result
}

fn detect_project_from_cwd(config: &Config) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    // Walk cwd and up to 3 ancestors, check if any dir name matches a memory project
    for ancestor in cwd.ancestors().take(4) {
        let name = ancestor.file_name()?.to_string_lossy().to_string();
        if config.memory_root.join(&name).exists() {
            return Some(name);
        }
    }
    None
}

/// Dispatch a passthrough subcommand (`orca <domain> <verb> [args]`) to the
/// `OrcaOp` inventory in `dispatch::cli`. Returns an error if no
/// (domain, verb) pair matches; clap printed help is preferred over this.
async fn dispatch_op(mut argv: Vec<String>, config: Config) -> Result<()> {
    use dispatch::cli as op_cli;
    use std::sync::Arc;

    argv.insert(0, "orca".to_string());
    // Build the static op tree, then splice in the live, plugin-driven managed
    // units fetched from the running daemon (or the local catalog when the
    // daemon is down). `unit` is internal: each kind becomes a TOP-LEVEL command
    // (`orca vm …`, `orca lxc …`), so `orca vm list --help` reflects exactly
    // what's loaded at runtime — service discovery with type hints. A kind whose
    // name collides with an existing static command is skipped (static wins).
    let unit_ops = op_cli::fetch_unit_ops().await;
    let unit_kinds = dispatch::unit_surface::unit_kinds_from(&unit_ops);
    let mut root = op_cli::build_root(clap::Command::new("orca"));
    let existing: std::collections::HashSet<String> = root
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    for cmd in dispatch::unit_surface::unit_cli_commands_from(unit_ops) {
        if !existing.contains(cmd.get_name()) {
            root = root.subcommand(cmd);
        }
    }
    // Static top-level `orca diagnostics` (two fixed ops; findings vary by plugin).
    {
        let diag = dispatch::diagnostics_surface::diagnostics_cli_command();
        if !existing.contains(diag.get_name()) {
            root = root.subcommand(diag);
        }
    }
    // Static top-level `orca ups` (three fixed ops; providers vary by plugin).
    {
        let ups = dispatch::ups_surface::ups_cli_command();
        if !existing.contains(ups.get_name()) {
            root = root.subcommand(ups);
        }
    }
    let matches = match root.try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => e.exit(),
    };

    // Reuse the MCP path's ToolCtx builder so every service trait (Docker,
    // Plugins, McpRegistry, etc.) is registered exactly once. Dispatch goes
    // through `OrcaTool::run` directly, not the inventory walk.
    let ctx = Arc::new(mcp::build_tool_ctx(Arc::new(config)));

    // The managed-unit kinds route through the daemon's REST path, not the CliOp
    // inventory, so try them first (only claims top-level names in `unit_kinds`).
    if let Some(r) = op_cli::dispatch_unit(&matches, ctx.clone(), &unit_kinds).await {
        return r;
    }
    if let Some(r) = op_cli::dispatch_diagnostics(&matches, ctx.clone()).await {
        return r;
    }
    if let Some(r) = op_cli::dispatch_ups(&matches, ctx.clone()).await {
        return r;
    }

    match op_cli::try_dispatch(&matches, ctx).await {
        Some(r) => r,
        None => anyhow::bail!("no OrcaOp matched"),
    }
}

async fn cmd_admin(action: AdminAction) -> Result<()> {
    match action {
        AdminAction::ResetPassword {
            username,
            revoke_sessions,
        } => cmd_admin_reset_password(&username, revoke_sessions),
        AdminAction::StorageApply => cmd_admin_storage_apply().await,
    }
}

/// Read a `PrivilegedOp` (JSON) from stdin, execute it as root, and print the
/// `PrivilegedResult` (JSON) to stdout. The daemon (as the `orca` user) invokes
/// this via `sudo -n orca admin storage-apply`; the sudoers grant is scoped to
/// exactly this command. All decision-making happened daemon-side — this just
/// validates paths and executes.
async fn cmd_admin_storage_apply() -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read PrivilegedOp from stdin")?;
    let op: system::autofs::PrivilegedOp =
        serde_json::from_str(&buf).context("parse PrivilegedOp JSON")?;
    let result = system::autofs::execute_privileged(op).await;
    println!(
        "{}",
        serde_json::to_string(&result).context("serialize PrivilegedResult")?
    );
    Ok(())
}

fn cmd_admin_reset_password(username: &str, revoke_sessions: bool) -> Result<()> {
    use std::io::{IsTerminal, Read, Write};

    let conn = db::open_default().context("open orca.db")?;
    let row = db::users::find_auth_by_username(&conn, username)
        .context("lookup user")?
        .ok_or_else(|| anyhow::anyhow!("no such user: {username}"))?;

    // Read new password from stdin. If stdin is a TTY, prompt + hide echo.
    // Otherwise read a line (lets `echo newpw | orca admin reset-password u`
    // work in scripts, with the obvious caveat that argv-history isn't where
    // the secret lives in that flow).
    let mut new_pw = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("New password for {}: ", row.username);
        std::io::stderr().flush().ok();
        new_pw = rpassword::read_password().context("read password")?;
        eprint!("Confirm: ");
        std::io::stderr().flush().ok();
        let confirm = rpassword::read_password().context("read confirmation")?;
        if new_pw != confirm {
            anyhow::bail!("passwords do not match");
        }
    } else {
        std::io::stdin().read_to_string(&mut new_pw)?;
        new_pw = new_pw.trim_end_matches(['\r', '\n']).to_string();
    }
    if new_pw.len() < 8 {
        anyhow::bail!("password must be at least 8 characters");
    }

    let hash = auth::password::hash_password(&new_pw).context("hash password")?;
    let now = utils::time::now_rfc3339();
    let updated =
        db::users::set_password_hash(&conn, &row.id, &hash, &now).context("write new hash")?;
    anyhow::ensure!(updated, "user row vanished mid-operation");

    let revoked = if revoke_sessions {
        db::sessions::revoke_all_for_user(&conn, &row.id, &now).context("revoke sessions")?
    } else {
        0
    };

    println!(
        "password reset for {} (role={}, sessions_revoked={})",
        row.username, row.role, revoked
    );
    Ok(())
}
