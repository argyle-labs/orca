use anyhow::Result;
use clap::{Parser, Subcommand};
use orca::commands::{self as cmd, DaemonAction, HookAction, SpecAction};
use orca::context::ProjectContext;
use orca::conversation::session::Session;
use orca::llm::{ClaudeBackend, Message, ModelBackend, stdout_sink};
use orca::log_cmd::{LogAction, cmd_log};
use orca::mcp;
use orca::serve;
use orca::serve::openapi_spec_json;
use orca_utils::config::Config;

#[derive(Parser)]
#[command(name = "orca", about = "Context-first AI agent orchestrator", version)]
struct Cli {
    /// Project context to load (e.g. "meerkat"). Omit for general session.
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
        /// Port to listen on
        #[arg(short, long, default_value = "12000")]
        port: u16,
    },

    /// Run as daemon with cooperative port handoff (SIGUSR1 park / SIGUSR2 reclaim)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Start dev server, superseding any running daemon on the port.
    /// Parks the stable daemon, runs dev mode, reclaims on exit.
    Dev {
        #[arg(short, long, default_value = "12000")]
        port: u16,
    },

    /// Manage the external API spec registry (~/.orca/openapi/)
    Spec {
        #[command(subcommand)]
        action: SpecAction,
    },

    /// Check or apply binary updates from GitHub releases on the configured channel.
    ///
    /// With no flags: applies the latest update on the channel marker
    /// (~/.orca/channel). Use --channel to switch (also rewrites the marker).
    /// Use --check to preview (downloads + caches the .sha256 only).
    Update {
        /// Channel override: stable | rc | beta | alpha. Falls back to the
        /// channel marker, then to "stable" if no marker is set.
        #[arg(long)]
        channel: Option<String>,
        /// Preview only — resolve the target version + cache its sha256,
        /// do not download or swap the binary.
        #[arg(long)]
        check: bool,
        /// Pin to a version. Future `orca update` runs will not upgrade past this.
        #[arg(long, value_name = "VERSION", conflicts_with = "unpin")]
        pin: Option<String>,
        /// Clear the version pin. `orca update` resumes following the channel.
        #[arg(long, conflicts_with = "pin")]
        unpin: bool,
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

    /// Passthrough for `OrcaOp`-migrated domains — dispatched via inventory.
    /// Captures any first arg not matching a derive variant above; the
    /// `orca-tools-def::cli` registry routes it to the right tool.
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
    /// specific addr `host[:port]`.
    Connect { addr: String },
    /// Manually push an offer to a known address (inviter side, when
    /// mDNS doesn't see the joiner).
    Offer { addr: String },
    /// List known peers and their trust state.
    List,
    /// Mark a peer as locally trusted (or untrust). Triggers CA-key
    /// replication when both sides have flagged each other secure.
    Trust {
        /// Peer ID (e.g. `peer.thor`).
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ORCA_LOG").unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,orca=info,tower_http=warn,axum=warn")
            }),
        )
        .with_target(false)
        .compact()
        .with_writer(std::io::stderr)
        .init();

    // Short-circuit OrcaOp ops *before* clap parse: the derive `Cli` has a
    // positional `project: Option<String>` that would otherwise swallow the
    // domain name (`orca engine list` → project=engine, command="list").
    //
    // Require both domain AND verb to be registered (or `--help`) — that way
    // legacy subcommands like `orca spec dump` still fall through to the
    // derive parser when their verb isn't a migrated OrcaOp.
    {
        let argv: Vec<String> = std::env::args().collect();
        if let Some(dom) = argv.get(1) {
            let verb_opt = argv.get(2);
            let is_domain_help =
                matches!(verb_opt.map(String::as_str), Some("--help") | Some("-h"));
            let matched = orca_tools_def::cli::ops().any(|o| {
                o.domain == dom
                    && (is_domain_help
                        || verb_opt.is_none()
                        || verb_opt.is_some_and(|v| o.verb == v))
            });
            if matched {
                let config = Config::load()?;
                let rest = argv[1..].to_vec();
                return dispatch_op(rest, config).await;
            }
        }
    }

    let cli = Cli::parse();

    // Dispatch hook commands before Config::load() — hooks run in a subprocess context
    // where Keychain access (called inside Config::load) can hang and trigger a SIGKILL timeout.
    // Hook implementations are lightweight (regex, stdin, filesystem) and don't need Config.
    if let Some(Command::Hook { action }) = cli.command {
        return cmd::cmd_hook(action);
    }

    let mut config = Config::load()?;
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
        Some(Command::Serve { dev, port }) => serve::run(dev, port, config.db_path.clone()).await,
        Some(Command::Daemon { action }) => match action {
            DaemonAction::Start { port } => serve::run_daemon(port, config.db_path.clone()).await,
            other => cmd::cmd_daemon(other),
        },
        Some(Command::Dev { port }) => cmd_dev(port, &config).await,
        Some(Command::Hook { action }) => cmd::cmd_hook(action),
        Some(Command::Op(argv)) => dispatch_op(argv, config).await,
        Some(Command::Update {
            channel,
            check,
            pin,
            unpin,
        }) => {
            if let Some(v) = pin {
                let pinned = cmd::update::cmd_update_pin(&v)?;
                println!("[orca] pinned to {pinned}");
                Ok(())
            } else if unpin {
                cmd::update::cmd_update_unpin()?;
                println!("[orca] pin cleared");
                Ok(())
            } else if check {
                cmd::update::cmd_update_check(channel.as_deref().unwrap_or("")).await
            } else {
                cmd::update::cmd_update(channel.as_deref().unwrap_or("")).await
            }
        }
        Some(Command::Pod { action }) => match action {
            PodAction::Init => {
                let pki = orca::pod::pki_dir();
                let host = std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                orca_sdk::pki::init_mesh_ca(&pki, &host)?;
                // Ensure the bootstrap identity (Ed25519 key + self-signed
                // cert) is present from the moment this host is poddable.
                orca_sdk::pki::load_or_init_bootstrap_cert(&pki)?;
                let conn = db::open_default()?;
                orca::pod::db::set_self_secure(&conn, true)?;
                let pod_id = format!("pod-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                orca::pod::db::set_pod_id(&conn, &pod_id)?;
                println!("✓ mesh CA initialized at {}", pki.join("mesh").display());
                println!("  pod id: {pod_id}");
                println!("  founder host CN: peer.{host}");
                println!("  self_secure: true (secrets storage enabled)");
                println!(
                    "  next: start the daemon. Auto-offers will flow to any \
                     unclaimed orca on the LAN; user accepts on the joiner with \
                     `orca pod accept <code>` (the code is printed in the daemon log here)."
                );
                Ok(())
            }
            PodAction::Ping { host } => {
                let result = orca::pod::ping(&host).await?;
                println!("✓ {host} responded:");
                println!("  peer_id: {}", result.peer_id);
                println!("  hostname: {}", result.hostname);
                println!("  version: {}", result.version);
                Ok(())
            }
            PodAction::Discover => cmd::pod::cmd_pod_discover(),
            PodAction::Pending => cmd::pod::cmd_pod_pending(),
            PodAction::Accept { code } => cmd::pod::cmd_pod_accept(&code).await,
            PodAction::Connect { addr } => cmd::pod::cmd_pod_connect(&addr).await,
            PodAction::Offer { addr } => cmd::pod::cmd_pod_offer(&addr).await,
            PodAction::List => cmd::pod::cmd_pod_list(),
            PodAction::Trust { peer_id, state } => {
                cmd::pod::cmd_pod_trust(&peer_id, state == "on").await
            }
            PodAction::SelfSecure { state } => {
                use cmd::pod::SelfSecureAction;
                let action = match state.as_str() {
                    "on" => SelfSecureAction::On,
                    "off" => SelfSecureAction::Off,
                    _ => SelfSecureAction::Show,
                };
                cmd::pod::cmd_pod_self_secure(action)
            }
            PodAction::Leave {
                wipe_secrets,
                wipe_all,
            } => cmd::pod::cmd_pod_leave(wipe_secrets, wipe_all).await,
        },
        Some(Command::Spec { action }) => match action {
            SpecAction::Dump => {
                let spec = openapi_spec_json();
                println!("{}", serde_json::to_string_pretty(&spec)?);
                Ok(())
            }
            other => cmd::cmd_spec(other),
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

/// Direct Claude escalation — loads project context if provided, then sends question.
/// On first run, ensure the implicit local user has a `default` profile and
/// that personal-classified agents are migrated into it from the embedded
/// baseline. Subsequent runs are a no-op once both have happened.
fn bootstrap_default_profile(config: &Config) -> Result<()> {
    let conn = db::open(&config.db_path)?;
    let mgr = orca::profile::ProfileManager::from_config(config);
    let p = mgr.ensure_default_for(&conn, orca_utils::config::LOCAL_USER)?;
    let n = mgr.migrate_personal_agents(&conn, &p)?;
    if n > 0 {
        tracing::info!(
            profile_id = %p.id,
            agents = n,
            "migrated personal agents into default profile"
        );
    }
    tracing::debug!(profile_id = %p.id, "active profile resolved");
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

/// Return true if something is already listening on `port`.
fn port_in_use(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(100),
    )
    .is_ok()
}

/// Park the stable daemon (if running), start dev server, reclaim on exit.
async fn cmd_dev(port: u16, config: &Config) -> Result<()> {
    use orca_utils::state::DaemonMode;
    use std::process::Command;

    // Spawn Vite dev server if not already running on 12001
    let vite_child = if !port_in_use(12001) {
        let frontend_dir = std::env::current_dir()
            .unwrap_or_default()
            .join("projects/frontend");
        if frontend_dir.exists() {
            println!("[orca] starting vite dev server...");
            // process_group(0) puts vite in its own process group so Ctrl-C
            // (SIGINT to orca's foreground group) does not kill vite.
            #[cfg(unix)]
            let mut cmd = {
                use std::os::unix::process::CommandExt;
                let mut c = Command::new("npm");
                c.args(["run", "dev"])
                    .current_dir(&frontend_dir)
                    .process_group(0);
                c
            };
            #[cfg(not(unix))]
            let mut cmd = {
                let mut c = Command::new("npm");
                c.args(["run", "dev"]).current_dir(&frontend_dir);
                c
            };
            match cmd.spawn() {
                Ok(child) => {
                    println!("[orca] vite started (pid {})", child.id());
                    Some(child)
                }
                Err(e) => {
                    eprintln!("[orca] warning: could not start vite: {e}");
                    None
                }
            }
        } else {
            eprintln!("[orca] warning: projects/frontend not found — run from orca workspace root");
            None
        }
    } else {
        println!("[orca] vite already running on :12001");
        None
    };

    // Park daemon if it's running
    let (daemon_pid, daemon_binary) = match orca_utils::state::read()? {
        Some(s) if s.mode == DaemonMode::Daemon => {
            // Capture binary now — state file may be gone by the time we need it
            let binary = s.binary.clone();
            let pid = s.daemon_pid;
            Command::new("kill")
                .args(["-USR1", &pid.to_string()])
                .status()?;
            if let Err(e) = orca_utils::state::wait_for_mode(DaemonMode::Parked, 5).await {
                // Parking timed out — reclaim immediately so daemon isn't stuck parked
                let _ = Command::new("kill")
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
    if let Some(mut s) = orca_utils::state::read()? {
        s.mode = DaemonMode::Dev;
        s.active_pid = std::process::id();
        let _ = orca_utils::state::write(&s);
    }

    // Run dev server (Ctrl-C will exit)
    let result = serve::run(true, port, config.db_path.clone()).await;

    // Leave vite running so the browser stays alive across orca restarts.
    // port_in_use(12001) prevents double-spawning on next `orca dev`.
    drop(vite_child);

    // Reclaim: read current state (daemon may have been restarted by launchd with a new PID)
    if daemon_pid.is_some() {
        let current_pid = orca_utils::state::read()
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
            let binary = orca_utils::state::read()
                .ok()
                .flatten()
                .map(|s| s.binary)
                .or(daemon_binary);
            if let Some(bin) = binary {
                println!("[orca] daemon gone — respawning {bin}");
                let _ = Command::new(&bin)
                    .args(["daemon", "start", "--port", &port.to_string()])
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
/// `OrcaOp` inventory in `orca-tools-def::cli`. Returns an error if no
/// (domain, verb) pair matches; clap printed help is preferred over this.
async fn dispatch_op(mut argv: Vec<String>, config: Config) -> Result<()> {
    use orca_tools_def::cli as op_cli;
    use std::sync::Arc;

    argv.insert(0, "orca".to_string());
    let root = op_cli::build_root(clap::Command::new("orca"));
    let matches = match root.try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => e.exit(),
    };

    // Reuse the MCP path's ToolCtx builder so every service trait (Docker,
    // Plugins, McpRegistry, etc.) is registered exactly once. The discarded
    // registry isn't needed for CLI — dispatch goes through `OrcaTool::run`
    // directly, not the registry walk.
    let (_reg, ctx) = mcp::build_tool_registry(Arc::new(config));
    let ctx = Arc::new(ctx);

    match op_cli::try_dispatch(&matches, ctx).await {
        Some(r) => r,
        None => anyhow::bail!("no OrcaOp matched"),
    }
}
