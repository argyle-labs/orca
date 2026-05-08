use anyhow::Result;
use orca::context::ProjectContext;
use orca::mcp;
use orca::serve;
use orca::serve::openapi_spec_json;
use orca::conversation::Session;
use orca_commands::{self as cmd, CredsAction, DaemonAction, DbAction, DockerAction, EnginesAction, HookAction, McpAction, PluginAction, SchemaAction, SpecAction, cmd_oauth_github, cmd_oauth_atlassian, cmd_logout_github, cmd_logout_atlassian, cmd_install, cmd_uninstall};
use orca::log_cmd::{LogAction, cmd_log};
use llm::{ClaudeBackend, ModelBackend, stdout_sink, Message};
use config::Config;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "orca", about = "Context-first AI agent orchestrator", version)]
struct Cli {
    /// Project context to load (e.g. "halvor"). Omit for general session.
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
    /// Authenticate with a service and store tokens in keychain
    Login {
        #[command(subcommand)]
        service: LoginService,
    },

    /// Check authentication and connectivity status
    Auth,

    /// Remove stored credentials from keychain
    Logout {
        #[command(subcommand)]
        service: LoginService,
    },

    /// List projects (memory dirs in orca vault)
    Projects,

    /// List available agents
    Agents,

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

    /// Validate agent files, symlinks, and config
    Doctor,

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

    /// Generate TypeScript types and hooks from the OpenAPI schema
    Gen {
        /// Backend URL to fetch the spec from
        #[arg(long, default_value = "http://localhost:12000")]
        url: String,
        /// Output directory (relative to frontend/)
        #[arg(long, default_value = "src/lib/api")]
        out: String,
    },

    /// Manage the external API spec registry (~/orca/openapi/)
    Spec {
        #[command(subcommand)]
        action: SpecAction,
    },

    /// Claude Code hook handlers (session-start, bash-guard, pii-scan, etc.)
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Manage MCP servers registered with orca
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Manage schema databases registered with orca
    Schema {
        #[command(subcommand)]
        action: SchemaAction,
    },

    /// Manage LLM backends (LM Studio, Ollama) registered with orca
    Engines {
        #[command(subcommand)]
        action: EnginesAction,
    },

    /// Manage Docker runtimes registered with orca
    Docker {
        #[command(subcommand)]
        action: DockerAction,
    },

    /// Manage orca plugins (register, list, enable/disable)
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },

    /// Manage plugin credentials — store in Orca, sync to plugins
    Creds {
        #[command(subcommand)]
        action: CredsAction,
    },

    /// Manage the orca.db schema (migrate, status)
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// Check for and apply updates from GitHub releases
    Update {
        /// Release channel: stable (default), rc, beta, alpha
        #[arg(long, default_value = "stable")]
        channel: String,
    },

    /// Install orca: wire symlinks, register MCP server, install binary
    Install,

    /// Uninstall orca: remove binary, MCP registration, and CLAUDE.md symlinks
    Uninstall,
}

#[derive(Subcommand)]
enum LoginService {
    /// Store Anthropic API key for Claude escalation
    Anthropic,
    /// Authenticate with GitHub via device flow
    Github,
    /// Authenticate with Atlassian (Jira + Confluence) via OAuth
    Atlassian,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ORCA_LOG")
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "warn,orca=info,tower_http=warn,axum=warn",
                    )
                }),
        )
        .with_target(false)
        .compact()
        .with_writer(std::io::stderr)
        .init();

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

    match cli.command {
        Some(Command::Login { service }) => match service {
            LoginService::Anthropic => cmd::cmd_login(&config),
            LoginService::Github => cmd_oauth_github().await,
            LoginService::Atlassian => cmd_oauth_atlassian().await,
        },
        Some(Command::Logout { service }) => match service {
            LoginService::Anthropic => { let _ = cmd::cmd_logout(); Ok(()) },
            LoginService::Github => cmd_logout_github(),
            LoginService::Atlassian => cmd_logout_atlassian(),
        },
        Some(Command::Auth) => cmd::cmd_auth(&config),
        Some(Command::Projects) => cmd::cmd_projects(&config),
        Some(Command::Agents) => cmd::cmd_agents(&config),
        Some(Command::Escalate { question, project }) => {
            escalate(&config, &question, project.as_deref()).await
        }
        Some(Command::Doctor) => cmd::cmd_doctor(&config),
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
        Some(Command::Mcp { action }) => cmd::cmd_mcp(action),
        Some(Command::Schema { action }) => cmd::cmd_schema(action),
        Some(Command::Engines { action }) => cmd::cmd_engines(action),
        Some(Command::Docker { action }) => cmd::cmd_docker(action),
        Some(Command::Plugin { action }) => cmd::cmd_plugin(action),
        Some(Command::Creds { action }) => cmd::cmd_creds(action),
        Some(Command::Db { action }) => cmd::cmd_db(action),
        Some(Command::Update { channel }) => {
            let ch = orca_commands::update::Channel::parse(&channel);
            cmd::cmd_update(ch).await
        }
        Some(Command::Install) => cmd_install(),
        Some(Command::Uninstall) => cmd_uninstall(),
        Some(Command::Gen { url, out }) => cmd::cmd_gen(&url, &out).await,
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
    claude.chat(&messages, &[], &system, cancel, &output).await?;
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
    use state::{self, DaemonMode};
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
                c.args(["run", "dev"]).current_dir(&frontend_dir).process_group(0);
                c
            };
            #[cfg(not(unix))]
            let mut cmd = {
                let mut c = Command::new("npm");
                c.args(["run", "dev"]).current_dir(&frontend_dir);
                c
            };
            match cmd.spawn()
            {
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
    let (daemon_pid, daemon_binary) = match state::read()? {
        Some(s) if s.mode == DaemonMode::Daemon => {
            // Capture binary now — state file may be gone by the time we need it
            let binary = s.binary.clone();
            let pid = s.daemon_pid;
            Command::new("kill")
                .args(["-USR1", &pid.to_string()])
                .status()?;
            if let Err(e) = state::wait_for_mode(DaemonMode::Parked, 5).await {
                // Parking timed out — reclaim immediately so daemon isn't stuck parked
                let _ = Command::new("kill").args(["-USR2", &pid.to_string()]).status();
                return Err(e.context("daemon did not park in time; reclaim sent"));
            }
            println!("[orca] daemon parked — dev server taking port {port}");
            (Some(pid), Some(binary))
        }
        _ => (None, None),
    };

    // Mark ourselves as the active dev process
    if let Some(mut s) = state::read()? {
        s.mode = DaemonMode::Dev;
        s.active_pid = std::process::id();
        let _ = state::write(&s);
    }

    // Run dev server (Ctrl-C will exit)
    let result = serve::run(true, port, config.db_path.clone()).await;

    // Leave vite running so the browser stays alive across orca restarts.
    // port_in_use(12001) prevents double-spawning on next `orca dev`.
    drop(vite_child);

    // Reclaim: read current state (daemon may have been restarted by launchd with a new PID)
    if daemon_pid.is_some() {
        let current_pid = state::read()
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
            let binary = state::read()
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
