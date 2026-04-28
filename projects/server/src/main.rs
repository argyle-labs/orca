use anyhow::Result;
use brain::agents;
use brain::auth;
use brain::backend;
use brain::config::Config;
use brain::context::ProjectContext;
use brain::log;
use brain::mcp;
use brain::scanner;
use brain::serve;
use brain::session::Session;
use clap::{Parser, Subcommand};
use colored::Colorize;

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

#[derive(Subcommand)]
enum SpecAction {
    /// List all registered external specs
    List,
    /// Register a repo and scaffold a spec file if one doesn't exist
    Add {
        /// Repository name (e.g. admin-api)
        repo: String,
        /// Project the repo belongs to (e.g. rebuy)
        #[arg(long, default_value = "rebuy")]
        project: String,
        /// Base URL for the API (e.g. https://api.example.com)
        #[arg(long)]
        url: Option<String>,
        /// Short description
        #[arg(long)]
        description: Option<String>,
    },
    /// [reserved] Snapshot a live API's OpenAPI output — not yet implemented
    Sync { repo: String },
}

#[derive(Subcommand)]
enum LogAction {
    /// Search all session logs for a keyword
    Search { query: String },
    /// List recent sessions
    Sessions {
        /// Max sessions to show
        #[arg(short, long, default_value = "15")]
        limit: usize,
    },
    /// Recall messages from a session
    Recall { session_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BRAIN_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
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
        Some(Command::Doctor) => cmd_doctor(&config),
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
            cmd_run(&config, "bear", &prompt).await
        }
        Some(Command::Run { agent, prompt }) => cmd_run(&config, &agent, &prompt).await,
        Some(Command::InstallAgents) => cmd_install_agents(&config),
        Some(Command::McpServe) => mcp::serve(&config).await,
        Some(Command::Serve { dev, port }) => serve::run(dev, port).await,
        Some(Command::Gen { url, out }) => cmd_gen(&url, &out).await,
        Some(Command::Spec { action }) => cmd_spec(action),
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

// ─── auth commands ────────────────────────────────────────────────────────────

fn cmd_login(config: &Config) -> Result<()> {
    if let Some(key) = &config.anthropic_api_key {
        println!(
            "{} API key already set: {}",
            "✓".green(),
            auth::mask_key(key).dimmed()
        );
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
    println!(
        "{}",
        "  Get one at: https://console.anthropic.com/settings/keys".dimmed()
    );
    print!("> ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let key = rpassword_or_stdin()?;
    let key = key.trim().to_string();

    if !key.starts_with("sk-ant-") {
        eprintln!(
            "{}",
            "key doesn't look right (expected sk-ant-…) — saving anyway".yellow()
        );
    }

    auth::store_api_key(&key)?;
    println!("{}", "API key stored in macOS Keychain.".green());
    println!(
        "{}",
        "Use /escalate or /model claude-* in sessions.".dimmed()
    );
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
            println!(
                "{} Anthropic API key: {}",
                "✓".green(),
                auth::mask_key(key).dimmed()
            );
        }
        None => {
            println!("{} Anthropic API key: not set", "✗".red());
            println!(
                "{}",
                "  run `brain login` to store one for Claude escalation".dimmed()
            );
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
        println!(
            "{}",
            "  enable Local Server in LM Studio → Developer tab".dimmed()
        );
    }

    Ok(())
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
    let mut entries: Vec<_> = std::fs::read_dir(root)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let marker = if e.path().join("MEMORY.md").exists() {
            "●"
        } else {
            "○"
        };
        println!("  {marker}  {name}");
    }
    Ok(())
}

fn cmd_agents(_config: &Config) -> Result<()> {
    println!("{}", "Agents:".green());
    for (name, desc) in agents::list_embedded_agents() {
        let short: String = desc.chars().take(72).collect();
        let ellipsis = if desc.len() > 72 { "…" } else { "" };
        println!(
            "  {}  {}{}",
            format!("@{name:<10}").cyan(),
            short.dimmed(),
            ellipsis.dimmed()
        );
    }
    Ok(())
}

fn cmd_install_agents(config: &Config) -> Result<()> {
    let target = config.agents_dir();
    println!(
        "{} installing agents into {}",
        "brain".cyan(),
        target.display()
    );

    let report = agents::install_agents(&target)?;

    for name in &report.written {
        println!("  {} {name}", "↑".green());
    }
    for name in &report.removed {
        println!("  {} {name}", "✗".red());
    }

    println!(
        "\n{} {} written, {} removed, {} unchanged",
        "✓".green(),
        report.written.len(),
        report.removed.len(),
        report.unchanged,
    );
    Ok(())
}

fn cmd_doctor(config: &Config) -> Result<()> {
    let mut issues: Vec<String> = Vec::new();
    let mut ok_count = 0;

    // 1. Brain vault exists
    if config.brain_vault.exists() {
        println!(
            "  {} brain vault: {}",
            "✓".green(),
            config.brain_vault.display()
        );
        ok_count += 1;
    } else {
        issues.push(format!(
            "brain vault not found at {}",
            config.brain_vault.display()
        ));
    }

    // 2. Agents dir exists and has files
    let agents_dir = config.agents_dir();
    let agent_files: Vec<_> = if agents_dir.exists() {
        std::fs::read_dir(&agents_dir)?
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .collect()
    } else {
        issues.push(format!("agents dir not found: {}", agents_dir.display()));
        vec![]
    };

    // 3. Validate each agent file has required frontmatter
    let mut agent_names: Vec<String> = Vec::new();
    for entry in &agent_files {
        let path = entry.path();
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        agent_names.push(stem.clone());

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let has_name = content.contains("name:");
        let has_desc = content.contains("description:");
        let has_tools = content.contains("tools:");

        if !has_name || !has_desc || !has_tools {
            let missing: Vec<&str> = [
                if !has_name { Some("name") } else { None },
                if !has_desc { Some("description") } else { None },
                if !has_tools { Some("tools") } else { None },
            ]
            .into_iter()
            .flatten()
            .collect();
            issues.push(format!(
                "{}.md: missing frontmatter: {}",
                stem,
                missing.join(", ")
            ));
        } else {
            ok_count += 1;
        }
    }
    println!(
        "  {} {} agent definitions found",
        "✓".green(),
        agent_files.len()
    );

    // 4. Cross-reference wolf.md routing table
    let wolf_path = agents_dir.join("wolf.md");
    if wolf_path.exists() {
        let wolf_content = std::fs::read_to_string(&wolf_path)?;
        // Extract agent names from wolf's table (lines like "| **@name** |")
        let wolf_agents: Vec<String> = wolf_content
            .lines()
            .filter_map(|line| {
                if let Some(start) = line.find("**@") {
                    let rest = &line[start + 3..];
                    rest.find("**").map(|end| rest[..end].to_string())
                } else {
                    None
                }
            })
            .collect();

        // Agents with files but not in wolf's table
        for name in &agent_names {
            if name != "wolf" && !wolf_agents.contains(name) {
                issues.push(format!(
                    "{}.md exists but not in wolf.md routing table",
                    name
                ));
            }
        }

        // Agents in wolf's table but no file
        for name in &wolf_agents {
            if !agent_names.contains(name) {
                issues.push(format!(
                    "@{} in wolf.md routing table but no {}.md file",
                    name, name
                ));
            }
        }

        if wolf_agents.len() == agent_names.len() - 1 {
            // -1 because wolf itself isn't in its own table
            ok_count += 1;
        }
    }

    // 5. Logs dir exists and is writable
    let logs_dir = config.logs_dir();
    if logs_dir.exists() {
        let test_file = logs_dir.join(".doctor_test");
        match std::fs::write(&test_file, "test") {
            Ok(_) => {
                let _ = std::fs::remove_file(&test_file);
                println!("  {} logs dir: writable", "✓".green());
                ok_count += 1;
            }
            Err(_) => issues.push(format!("logs dir not writable: {}", logs_dir.display())),
        }
    } else {
        issues.push(format!("logs dir not found: {}", logs_dir.display()));
    }

    // 6. Memory root exists
    if config.memory_root.exists() {
        let project_count = std::fs::read_dir(&config.memory_root)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .count();
        println!("  {} memory root: {} projects", "✓".green(), project_count);
        ok_count += 1;
    } else {
        issues.push(format!(
            "memory root not found: {}",
            config.memory_root.display()
        ));
    }

    // 7. API key
    if config.anthropic_api_key.is_some() {
        println!("  {} anthropic API key: set", "✓".green());
        ok_count += 1;
    } else {
        println!(
            "  {} anthropic API key: not set (escalation unavailable)",
            "⚠".yellow()
        );
    }

    // Report
    println!();
    if issues.is_empty() {
        println!(
            "{}",
            format!("  all clear — {} checks passed", ok_count).green()
        );
    } else {
        println!("{}", format!("  {} issue(s) found:", issues.len()).red());
        for issue in &issues {
            println!("    {} {}", "✗".red(), issue);
        }
        println!();
        println!("  {} checks passed", ok_count);
    }

    Ok(())
}

fn cmd_log(config: &Config, action: LogAction) -> Result<()> {
    let logs_dir = config.logs_dir();

    match action {
        LogAction::Search { query } => match log::search_logs(&logs_dir, &query, 20) {
            Ok(matches) if matches.is_empty() => {
                println!("{}", format!("no matches for '{query}'").dimmed());
            }
            Ok(matches) => {
                println!("{}", format!("found {} match(es):", matches.len()).green());
                for m in &matches {
                    let session = m["session"].as_str().unwrap_or("?");
                    let role = m["role"].as_str().unwrap_or("?");
                    let agent = m["agent"].as_str().unwrap_or("?");
                    let content = m["content"].as_str().unwrap_or("");
                    let preview: String = content.chars().take(120).collect();
                    let important = m["important"].as_bool() == Some(true);
                    let flag = if important { " ★" } else { "" };
                    println!(
                        "  {} {} @{} {}{}",
                        session.dimmed(),
                        role.cyan(),
                        agent,
                        preview,
                        flag.yellow()
                    );
                }
            }
            Err(e) => eprintln!("{}", format!("search error: {e}").red()),
        },
        LogAction::Sessions { limit } => match log::list_sessions(&logs_dir, limit) {
            Ok(sessions) if sessions.is_empty() => {
                println!("{}", "no sessions found".dimmed());
            }
            Ok(sessions) => {
                println!("{}", "Recent sessions:".green());
                for s in &sessions {
                    let flag = if s.flagged > 0 {
                        format!(" (★ {})", s.flagged)
                    } else {
                        String::new()
                    };
                    println!(
                        "  {}  {} msgs{}",
                        s.session_id.dimmed(),
                        s.messages,
                        flag.yellow()
                    );
                }
            }
            Err(e) => eprintln!("{}", format!("error: {e}").red()),
        },
        LogAction::Recall { session_id } => match log::recall_session(&logs_dir, &session_id) {
            Ok(records) => {
                println!(
                    "{}",
                    format!("session: {} ({} records)", session_id, records.len()).green()
                );
                for r in &records {
                    let role = r["role"].as_str().unwrap_or("?");
                    let agent = r["agent"].as_str().unwrap_or("");
                    let content = r["content"].as_str().unwrap_or("");
                    let important = r["important"].as_bool() == Some(true);
                    let flag = if important { " ★" } else { "" };
                    let prefix = if agent.is_empty() {
                        format!("[{role}]")
                    } else {
                        format!("[{role}/@{agent}]")
                    };
                    let preview: String = content.chars().take(200).collect();
                    let ellipsis = if content.len() > 200 { "…" } else { "" };
                    println!(
                        "  {} {}{}{}",
                        prefix.cyan(),
                        preview,
                        ellipsis,
                        flag.yellow()
                    );
                }
            }
            Err(e) => eprintln!("{}", format!("error: {e}").red()),
        },
    }

    Ok(())
}

async fn cmd_run(config: &Config, agent: &str, prompt: &str) -> Result<()> {
    let ctx = ProjectContext::default();
    let mut session = Session::new(config.clone(), ctx).await?;

    // If a specific agent was requested, wrap as a delegation task
    if agent != "wolf" && agent != "brain" {
        let delegate_prompt = format!("Delegate this to @{agent}: {prompt}");
        session.one_shot(delegate_prompt).await
    } else {
        session.one_shot(prompt.to_string()).await
    }
}

async fn cmd_escalate(config: &Config, question: &str, project: Option<&str>) -> Result<()> {
    use backend::ClaudeBackend;
    use backend::ModelBackend;

    let api_key = config
        .anthropic_api_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no API key — run `brain login`"))?;

    let system = if let Some(p) = project {
        ProjectContext::resolve(p, config)?.build_system_prompt(config)
    } else {
        String::new()
    };

    let claude = ClaudeBackend::new(api_key, "claude-sonnet-4-6");
    let messages = vec![brain::types::Message::user(question)];
    let cancel = tokio_util::sync::CancellationToken::new();
    let output = backend::stdout_sink();
    claude
        .chat(&messages, &[], &system, cancel, &output)
        .await?;
    Ok(())
}

fn cmd_spec(action: SpecAction) -> Result<()> {
    match action {
        SpecAction::List => {
            let registry = scanner::SpecRegistry::load()?;
            if registry.entries.is_empty() {
                println!(
                    "{}",
                    "no specs registered — use `brain spec add <repo>`".dimmed()
                );
                return Ok(());
            }
            println!("{}", "External specs:".green());
            for e in &registry.entries {
                let url = e.base_url.as_deref().unwrap_or("-");
                let captured = e.captured_at.as_deref().unwrap_or("-");
                println!(
                    "  {}  project={}  url={}  captured={}  [{}]",
                    e.repo.cyan(),
                    e.project.dimmed(),
                    url.dimmed(),
                    captured.dimmed(),
                    e.source.yellow(),
                );
            }
        }

        SpecAction::Add {
            repo,
            project,
            url,
            description,
        } => {
            let mut registry = scanner::SpecRegistry::load()?;
            let entry = scanner::SpecEntry {
                repo: repo.clone(),
                project,
                description,
                source: "manual".to_string(),
                base_url: url,
                captured_at: Some(chrono::Utc::now().to_rfc3339()),
            };
            let spec_path = registry.add(entry)?;
            println!(
                "{} registered {} → {}",
                "✓".green(),
                repo.cyan(),
                spec_path.display()
            );
            println!(
                "{}",
                "  edit the scaffolded spec manually, then restart `brain serve`".dimmed()
            );
        }

        SpecAction::Sync { repo } => {
            println!(
                "{}",
                format!("sync not yet implemented for '{repo}' — snapshot automation coming soon")
                    .yellow()
            );
            println!(
                "{}",
                format!("  manually update ~/brain/openapi/{repo}.json for now").dimmed()
            );
        }
    }
    Ok(())
}

async fn cmd_gen(url: &str, out: &str) -> Result<()> {
    use colored::Colorize;

    // Poll until the backend is reachable (up to 30s after a cargo-watch restart)
    let spec_url = format!("{url}/api/openapi.json");
    let client = reqwest::Client::new();
    let mut attempts = 0;
    loop {
        match client.get(&spec_url).send().await {
            Ok(r) if r.status().is_success() => break,
            _ => {
                attempts += 1;
                if attempts >= 30 {
                    anyhow::bail!("backend not reachable at {spec_url} after 30s");
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }

    // Run the TypeScript generator in frontend/
    let repo_root = std::env::current_exe()?
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();

    // Fall back to cwd-relative frontend/ if the exe path heuristic fails
    let site_dir = if repo_root.join("frontend/scripts/gen.ts").exists() {
        repo_root.join("frontend")
    } else {
        std::env::current_dir()?.join("frontend")
    };

    println!(
        "{} generating types and hooks from {spec_url}",
        "brain gen".cyan()
    );

    let status = std::process::Command::new("npx")
        .args(["tsx", "scripts/gen.ts", "--url", url, "--out", out])
        .current_dir(&site_dir)
        .status()?;

    if status.success() {
        println!("{} {}/{}", "✓".green(), site_dir.display(), out);
    } else {
        anyhow::bail!("generator failed — check frontend/scripts/gen.ts");
    }
    Ok(())
}
