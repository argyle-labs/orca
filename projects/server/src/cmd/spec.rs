use anyhow::Result;
use crate::scanner;
use crate::serve;
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

        SpecAction::Dump => {
            let spec = serve::openapi_spec_json();
            println!("{}", serde_json::to_string_pretty(&spec)?);
        }
    }
    Ok(())
}
