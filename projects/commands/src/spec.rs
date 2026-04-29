use anyhow::Result;
use brain_scanner as scanner;
use clap::Subcommand;
use colored::Colorize;

#[derive(Subcommand)]
pub enum SpecAction {
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

    /// Dump brain's own OpenAPI spec to stdout (no server required — used by `make build`)
    Dump,
}

pub fn cmd_spec(action: SpecAction) -> Result<()> {
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
            match repo.as_str() {
                "admin-api" => {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let rebuy_root = std::env::var("REBUY_ROOT")
                        .unwrap_or_else(|_| format!("{home}/code/rebuy"));
                    let repo_path = std::path::PathBuf::from(&rebuy_root).join("admin-api");
                    if !repo_path.exists() {
                        anyhow::bail!(
                            "admin-api not found at {} — set REBUY_ROOT or clone the repo",
                            repo_path.display()
                        );
                    }
                    print!("  scanning routes and schemas");
                    let spec = scanner::ci4_generator::generate(&repo_path)?;
                    let out_path = std::path::PathBuf::from(&home)
                        .join("brain/openapi/admin-api.json");
                    std::fs::create_dir_all(out_path.parent().unwrap())?;
                    std::fs::write(&out_path, serde_json::to_string_pretty(&spec)?)?;
                    let path_count = spec["paths"].as_object().map(|p| p.len()).unwrap_or(0);
                    let schema_count = spec["components"]["schemas"]
                        .as_object()
                        .map(|s| s.len())
                        .unwrap_or(0);
                    println!(
                        "\n{} synced admin-api → {} ({} paths, {} schemas)",
                        "✓".green(),
                        out_path.display(),
                        path_count,
                        schema_count,
                    );
                }
                "rebuyengine" => {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let rebuy_root = std::env::var("REBUY_ROOT")
                        .unwrap_or_else(|_| format!("{home}/code/rebuy"));
                    let repo_path = std::path::PathBuf::from(&rebuy_root).join("rebuyengine.com");
                    if !repo_path.exists() {
                        anyhow::bail!(
                            "rebuyengine.com not found at {} — set REBUY_ROOT or clone the repo",
                            repo_path.display()
                        );
                    }
                    print!("  scanning api.php dispatch chains");
                    let spec = scanner::ci2_generator::generate(&repo_path)?;
                    let out_path = std::path::PathBuf::from(&home)
                        .join("brain/openapi/rebuyengine.json");
                    std::fs::create_dir_all(out_path.parent().unwrap())?;
                    std::fs::write(&out_path, serde_json::to_string_pretty(&spec)?)?;
                    let path_count = spec["paths"].as_object().map(|p| p.len()).unwrap_or(0);
                    println!(
                        "\n{} synced rebuyengine → {} ({} paths)",
                        "✓".green(),
                        out_path.display(),
                        path_count,
                    );
                }
                _ => {
                    println!(
                        "{}",
                        format!("sync not implemented for '{repo}' — valid: admin-api, rebuyengine")
                            .yellow()
                    );
                    println!(
                        "{}",
                        format!("  manually update ~/brain/openapi/{repo}.json for now").dimmed()
                    );
                }
            }
        }

        SpecAction::Dump => {
            // Handled by the server binary (main.rs) which has access to brain::serve::openapi.
            // Should not reach here.
            anyhow::bail!("spec dump must be dispatched from main.rs")
        }
    }
    Ok(())
}
