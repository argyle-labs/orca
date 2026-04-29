use anyhow::Result;
use brain::context::ProjectContext;
use brain::mcp;
use brain::serve;
use brain::serve::openapi_spec_json;
use brain::session::Session;
use brain_commands::{self as cmd, DaemonAction, LogAction, McpAction, SpecAction};
use brain_core::backend::{ClaudeBackend, ModelBackend, stdout_sink};
use brain_utils::config::Config;
use brain_utils::types::Message;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "brain", about = "Context-first AI agent orchestrator", version)]
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
    /// Store Anthropic API key for Claude escalation
    Login,

    /// Check authentication and connectivity status
    Auth,

    /// Remove stored API key from keychain
    Logout,

    /// List projects (memory dirs in brain vault)
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

    /// Start MCP stdio server — exposes brain tools to Claude Code
    McpServe,

    /// Start the brain web server (docs + services UI)
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
        #[arg(long, default_value = "src/api")]
        out: String,
    },

    /// Install embedded agents into ~/.claude/agents/ (removes orphans from prior versions)
    InstallAgents,

    /// Manage the external API spec registry (~/brain/openapi/)
    Spec {
        #[command(subcommand)]
        action: SpecAction,
    },

    /// Manage MCP servers registered with brain
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BRAIN_LOG")
                .unwrap_or_else(|_| {
                    // Quiet external crates; only surface brain's own info/warn/error.
                    tracing_subscriber::EnvFilter::new(
                        "warn,brain=info,tower_http=warn,axum=warn",
                    )
                }),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    let config = Config::load()?;

    match cli.command {
        Some(Command::Login) => cmd::cmd_login(&config),
        Some(Command::Logout) => cmd::cmd_logout(),
        Some(Command::Auth) => cmd::cmd_auth(&config),
        Some(Command::Projects) => cmd::cmd_projects(&config),
        Some(Command::Agents) => cmd::cmd_agents(&config),
        Some(Command::Escalate { question, project }) => {
            escalate(&config, &question, project.as_deref()).await
        }
        Some(Command::Doctor) => cmd::cmd_doctor(&config),
        Some(Command::Log { action }) => cmd::cmd_log(&config, action),
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
        Some(Command::InstallAgents) => cmd::cmd_install_agents(&config),
        Some(Command::McpServe) => mcp::serve(&config).await,
        Some(Command::Serve { dev, port }) => serve::run(dev, port, config.mcp_servers.clone()).await,
        Some(Command::Daemon { action }) => match action {
            DaemonAction::Start { port } => serve::run_daemon(port, config.mcp_servers).await,
            other => cmd::cmd_daemon(other),
        },
        Some(Command::Dev { port }) => cmd_dev(port, &config).await,
        Some(Command::Mcp { action }) => cmd::cmd_mcp(&config, action),
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
        .ok_or_else(|| anyhow::anyhow!("no API key — run `brain login`"))?;

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

/// Park the stable daemon (if running), start dev server, reclaim on exit.
async fn cmd_dev(port: u16, config: &Config) -> Result<()> {
    use brain_utils::state::{self, DaemonMode};
    use std::process::Command;

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
            println!("[brain] daemon parked — dev server taking port {port}");
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
    let result = serve::run(true, port, config.mcp_servers.clone()).await;

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
            println!("[brain] daemon reclaimed port {port}");
        } else {
            // Daemon is not alive and was not restarted by launchd — spawn fresh
            let binary = state::read()
                .ok()
                .flatten()
                .map(|s| s.binary)
                .or(daemon_binary);
            if let Some(bin) = binary {
                println!("[brain] daemon gone — respawning {bin}");
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
