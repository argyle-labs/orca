mod agents;
mod auth;
mod backend;
mod config;
mod context;
mod ledger;
mod log;
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
#[command(name = "brain", about = "Context-first AI agent orchestrator", version)]
struct Cli {
    /// Project context to load (e.g. "halvor"). Omit for general session.
    #[arg(value_name = "PROJECT")]
    project: Option<String>,

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

    /// One-shot: send prompt to an agent and print response
    Run {
        #[arg(short = 'a', long, default_value = "wolf")]
        agent: String,
        prompt: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("BRAIN_LOG"))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config = Config::load()?;

    match cli.command {
        Some(Command::Login) => cmd_login(&config),
        Some(Command::Logout) => cmd_logout(),
        Some(Command::Auth) => cmd_auth(&config),
        Some(Command::Projects) => cmd_projects(&config),
        Some(Command::Agents) => cmd_agents(&config),
        Some(Command::Escalate { question, project }) => {
            cmd_escalate(&config, &question, project.as_deref()).await
        }
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
            cmd_run(&config, "bear", &prompt).await
        }
        Some(Command::Run { agent, prompt }) => {
            cmd_run(&config, &agent, &prompt).await
        }
        None => {
            let project = cli.project.as_deref().unwrap_or("");
            let ctx = if project.is_empty() {
                ProjectContext::default()
            } else {
                ProjectContext::resolve(project, &config)?
            };
            let mut session = Session::new(config, ctx).await?;
            session.run().await
        }
    }
}

// ─── auth commands ────────────────────────────────────────────────────────────

fn cmd_login(config: &Config) -> Result<()> {
    if let Some(key) = &config.anthropic_api_key {
        println!("{} API key already set: {}", "✓".green(), auth::mask_key(key).dimmed());
        println!("  {}  no", "[1]".dimmed());
        println!("  {}  yes, replace", "[2]".dimmed());
        print!("{} ", "[1]:".cyan());
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
        if input.trim() != "2" {
            return Ok(());
        }
    }

    println!("{}", "Enter your Anthropic API key (sk-ant-…):".cyan());
    println!("{}", "  Get one at: https://console.anthropic.com/settings/keys".dimmed());
    print!("> ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let key = rpassword_or_stdin()?;
    let key = key.trim().to_string();

    if !key.starts_with("sk-ant-") {
        eprintln!("{}", "key doesn't look right (expected sk-ant-…) — saving anyway".yellow());
    }

    auth::store_api_key(&key)?;
    println!("{}", "API key stored in macOS Keychain.".green());
    println!("{}", "Use /escalate or /model claude-* in sessions.".dimmed());
    Ok(())
}

fn cmd_logout() -> Result<()> {
    auth::remove_api_key();
    println!("{}", "API key removed from keychain.".green());
    Ok(())
}

fn cmd_auth(config: &Config) -> Result<()> {
    match &config.anthropic_api_key {
        Some(key) => {
            println!("{} Anthropic API key: {}", "✓".green(), auth::mask_key(key).dimmed());
        }
        None => {
            println!("{} Anthropic API key: not set", "✗".red());
            println!("{}", "  run `brain login` to store one for Claude escalation".dimmed());
        }
    }

    // LM Studio connectivity
    let lms_url = &config.lmstudio_url;
    let lms_status = std::process::Command::new("curl")
        .args(["-sf", &format!("{lms_url}/v1/models"), "-o", "/dev/null"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if lms_status {
        println!("{} LM Studio: reachable at {lms_url}", "✓".green());
    } else {
        println!("{} LM Studio: not reachable at {lms_url}", "✗".yellow());
        println!("{}", "  enable Local Server in LM Studio → Developer tab".dimmed());
    }

    Ok(())
}

fn rpassword_or_stdin() -> Result<String> {
    // Try to read without echo using stty
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut input = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!(); // newline after hidden input
    Ok(input)
}

// ─── listing commands ─────────────────────────────────────────────────────────

fn cmd_projects(config: &Config) -> Result<()> {
    let root = &config.memory_root;
    if !root.exists() {
        println!("{}", "brain vault not found at ~/brain".red());
        return Ok(());
    }
    println!("{}", "Projects:".green());
    let mut entries: Vec<_> = std::fs::read_dir(root)?.flatten()
        .filter(|e| e.path().is_dir()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let marker = if e.path().join("MEMORY.md").exists() { "●" } else { "○" };
        println!("  {marker}  {name}");
    }
    Ok(())
}

fn cmd_agents(config: &Config) -> Result<()> {
    let dir = config.agents_dir();
    if !dir.exists() {
        println!("{}", "agents dir not found".red());
        return Ok(());
    }
    println!("{}", "Agents:".green());
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?.flatten()
        .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let desc = read_frontmatter_field(&path, "description").unwrap_or_default();
        let short: String = desc.chars().take(72).collect();
        let ellipsis = if desc.len() > 72 { "…" } else { "" };
        println!("  {}  {}{}", format!("@{name:<10}").cyan(), short.dimmed(), ellipsis.dimmed());
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

async fn cmd_run(config: &Config, agent: &str, prompt: &str) -> Result<()> {
    use session::Session;

    let ctx = ProjectContext::default();
    let mut session = Session::new(config.clone(), ctx).await?;

    // If a specific agent was requested, wrap as a delegation task
    if agent != "wolf" && agent != "brain" {
        let delegate_prompt = format!(
            "Delegate this to @{agent}: {prompt}"
        );
        session.one_shot(delegate_prompt).await
    } else {
        session.one_shot(prompt.to_string()).await
    }
}

async fn cmd_escalate(config: &Config, question: &str, project: Option<&str>) -> Result<()> {
    use backend::ClaudeBackend;
    use backend::ModelBackend;

    let api_key = config.anthropic_api_key.clone()
        .ok_or_else(|| anyhow::anyhow!("no API key — run `brain login`"))?;

    let system = if let Some(p) = project {
        ProjectContext::resolve(p, config)?.build_system_prompt(config)
    } else {
        String::new()
    };

    let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
    let messages = vec![types::Message::user(question)];
    let cancel = tokio_util::sync::CancellationToken::new();
    claude.chat(&messages, &[], &system, cancel).await?;
    Ok(())
}
