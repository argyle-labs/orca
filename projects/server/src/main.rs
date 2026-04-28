use anyhow::Result;
use brain::context::ProjectContext;
use brain::mcp;
use brain::serve;
use brain::serve::openapi_spec_json;
use brain::session::Session;
use brain_commands::{self as cmd, LogAction, SpecAction};
use brain_utils::config::Config;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BRAIN_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_ansi(true)
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
            cmd::cmd_escalate(&config, &question, project.as_deref()).await
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
            cmd::cmd_run(&config, "bear", &prompt).await
        }
        Some(Command::Run { agent, prompt }) => cmd::cmd_run(&config, &agent, &prompt).await,
        Some(Command::InstallAgents) => cmd::cmd_install_agents(&config),
        Some(Command::McpServe) => mcp::serve(&config).await,
        Some(Command::Serve { dev, port }) => serve::run(dev, port).await,
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
