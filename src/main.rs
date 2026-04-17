mod backend;
mod config;
mod context;
mod ledger;
mod session;
mod tools;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::Colorize;
use config::Config;
use context::ProjectContext;
use session::Session;

#[derive(Parser)]
#[command(
    name = "brain",
    about = "Context-first AI agent orchestrator",
    version
)]
struct Cli {
    /// Project context to load (e.g. "halvor", "bardbase")
    /// If omitted, starts a general session.
    #[arg(value_name = "PROJECT")]
    project: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Send a one-shot message and print the response (non-interactive)
    Run {
        #[arg(short = 'a', long = "agent", default_value = "wolf")]
        agent: String,
        prompt: String,
    },

    /// Ask Claude a question (bypasses local model, useful for escalation)
    Escalate {
        question: String,
        #[arg(long)]
        project: Option<String>,
    },

    /// Log into Claude Code (runs: claude /login)
    Login,

    /// Check authentication status
    Auth,

    /// List all known projects (memory dirs in the brain vault)
    Projects,

    /// List all agents in the brain vault
    Agents,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging — only active when BRAIN_LOG is set
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("BRAIN_LOG"))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config = Config::load()?;

    match cli.command {
        Some(Command::Login) => cmd_login(),
        Some(Command::Auth) => cmd_auth(&config),
        Some(Command::Projects) => cmd_projects(&config),
        Some(Command::Agents) => cmd_agents(&config),
        Some(Command::Escalate { question, project }) => {
            cmd_escalate(&config, &question, project.as_deref()).await
        }
        Some(Command::Run { .. }) => {
            println!("{}", "brain run: coming in Phase 2".yellow());
            Ok(())
        }
        None => {
            let project = cli.project.as_deref().unwrap_or("");
            let ctx = if project.is_empty() {
                ProjectContext::default()
            } else {
                ProjectContext::resolve(project, &config)?
            };
            let mut session = Session::new(config, ctx)?;
            session.run().await
        }
    }
}

fn cmd_login() -> Result<()> {
    println!("{}", "Launching Claude Code login…".cyan());
    println!("{}", "(This opens a browser window for OAuth)".dimmed());

    let status = std::process::Command::new("claude").arg("/login").status();

    match status {
        Ok(s) if s.success() => {
            println!("{}", "Login successful.".green());
        }
        Ok(s) => {
            eprintln!("{}", format!("claude exited with code {:?}", s.code()).red());
        }
        Err(_) => {
            eprintln!("{}", "claude CLI not found in PATH.".red());
            eprintln!("{}", "Install Claude Code: https://claude.ai/code".dimmed());
            eprintln!("{}", "Or set ANTHROPIC_API_KEY to use the API directly.".dimmed());
        }
    }
    Ok(())
}

fn cmd_auth(config: &Config) -> Result<()> {
    match &config.anthropic_api_key {
        Some(key) => {
            let masked = format!("sk-ant-…{}", &key[key.len().saturating_sub(6)..]);
            println!("{} ANTHROPIC_API_KEY: {}", "✓".green(), masked.dimmed());
        }
        None => {
            println!("{} ANTHROPIC_API_KEY not set", "✗".red());
            println!(
                "{}",
                "  run `brain login` or set ANTHROPIC_API_KEY in your environment".dimmed()
            );
        }
    }

    println!("  LM Studio: {}", config.lmstudio_url.dimmed());

    let claude_ok = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .is_ok();
    if claude_ok {
        println!("{} claude CLI: found in PATH", "✓".green());
    } else {
        println!("{} claude CLI: not found (needed for `brain login`)", "✗".yellow());
    }

    Ok(())
}

fn cmd_projects(config: &Config) -> Result<()> {
    let memory_root = &config.memory_root;
    if !memory_root.exists() {
        println!("{}", "brain vault not found at ~/brain".red());
        return Ok(());
    }

    println!("{}", "Projects (~/brain/ai/claude/memory/):".green());
    let mut entries: Vec<_> = std::fs::read_dir(memory_root)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let has_memory = entry.path().join("MEMORY.md").exists();
        let marker = if has_memory { "●" } else { "○" };
        println!("  {marker}  {name}");
    }
    Ok(())
}

fn cmd_agents(config: &Config) -> Result<()> {
    let agents_dir = config.agents_dir();
    if !agents_dir.exists() {
        println!("{}", "agents dir not found at ~/brain/ai/claude/agents/".red());
        return Ok(());
    }

    println!("{}", "Agents (~/brain/ai/claude/agents/):".green());
    let mut entries: Vec<_> = std::fs::read_dir(&agents_dir)?
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "md")
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let desc = read_frontmatter_field(&path, "description").unwrap_or_default();
        // Truncate description for display
        let short: String = desc.chars().take(80).collect();
        let ellipsis = if desc.len() > 80 { "…" } else { "" };
        println!("  {}  {}{}", format!("@{name}").cyan(), short.dimmed(), ellipsis.dimmed());
    }
    Ok(())
}

fn read_frontmatter_field(path: &std::path::Path, field: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{field}:");
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

async fn cmd_escalate(
    config: &Config,
    question: &str,
    project: Option<&str>,
) -> Result<()> {
    use backend::ClaudeBackend;
    use backend::ModelBackend;

    let api_key = config.anthropic_api_key.clone().ok_or_else(|| {
        anyhow::anyhow!("ANTHROPIC_API_KEY not set. Run `brain login` to authenticate.")
    })?;

    let system = if let Some(p) = project {
        let ctx = ProjectContext::resolve(p, config)?;
        ctx.build_system_prompt(config)
    } else {
        String::new()
    };

    let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
    let messages = vec![types::Message::user(question)];
    let _response = claude.chat(&messages, &[], &system).await?;

    Ok(())
}
