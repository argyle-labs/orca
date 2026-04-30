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
    /// Generate an OpenAPI spec for a registered rebuy repo by scanning its
    /// source. Pass --all to sync every supported repo at once (used by
    /// `make build`). Repos that aren't checked out locally are skipped with
    /// a warning instead of failing the build.
    Sync {
        /// Repo to sync — admin-api | rebuyengine. Omit when --all is set.
        repo: Option<String>,
        /// Sync every supported repo. Skips repos that aren't checked out.
        #[arg(long)]
        all: bool,
    },

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

        SpecAction::Sync { repo, all } => {
            // Single command, two modes: --all syncs every supported repo
            // (skipping any that aren't checked out, so `make build` doesn't
            // fail on a fresh laptop). Otherwise the user named one repo.
            let repos: Vec<&str> = if all {
                vec![
                    "admin-api",
                    "apiv2",
                    "rebuyengine",
                    "admin-nextjs",
                    "rebuy-shopify-client",
                    "shopify-admin",
                ]
            } else {
                match repo.as_deref() {
                    Some(r) => vec![sync_known_repo(r)?],
                    None => anyhow::bail!("usage: brain spec sync <repo> | --all"),
                }
            };
            // `strict` controls whether a missing repo aborts the run. With
            // --all we never abort — just log and move on.
            let strict = !all;
            for r in repos {
                if let Err(e) = sync_one(r, strict) {
                    if strict { return Err(e); }
                    println!("{} {}: {}", "⊘".yellow(), r, e);
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

// ── Helpers ──────────────────────────────────────────────────────────────────

fn sync_known_repo(name: &str) -> Result<&str> {
    match name {
        "admin-api" => Ok("admin-api"),
        "apiv2" => Ok("apiv2"),
        "rebuyengine" => Ok("rebuyengine"),
        "admin-nextjs" => Ok("admin-nextjs"),
        "rebuy-shopify-client" => Ok("rebuy-shopify-client"),
        "shopify-admin" => Ok("shopify-admin"),
        other => anyhow::bail!(
            "sync not implemented for '{other}' — valid: admin-api, apiv2, rebuyengine, admin-nextjs, rebuy-shopify-client, shopify-admin"
        ),
    }
}

fn rebuy_repo_path(name: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let rebuy_root = std::env::var("REBUY_ROOT")
        .unwrap_or_else(|_| format!("{home}/code/rebuy"));
    std::path::PathBuf::from(&rebuy_root).join(name)
}

fn write_spec(repo: &str, spec: &serde_json::Value) -> Result<std::path::PathBuf> {
    let out_path = scanner::openapi_dir().join(format!("{repo}.json"));
    std::fs::create_dir_all(out_path.parent().unwrap())?;
    std::fs::write(&out_path, serde_json::to_string_pretty(spec)?)?;
    Ok(out_path)
}

fn sync_one(repo: &str, _strict: bool) -> Result<()> {
    match repo {
        "admin-api" => sync_ci4("admin-api", "admin-api"),
        "apiv2" => sync_ci4("apiv2", "apiv2"),
        "rebuyengine" => {
            let repo_path = rebuy_repo_path("rebuyengine.com");
            if !repo_path.exists() {
                anyhow::bail!(
                    "rebuyengine.com not found at {} — set REBUY_ROOT or clone the repo",
                    repo_path.display()
                );
            }
            print!("  scanning rebuyengine api.php dispatch chains");
            let spec = scanner::ci2_generator::generate(&repo_path)?;
            let out_path = write_spec("rebuyengine", &spec)?;
            let path_count = spec["paths"].as_object().map(|p| p.len()).unwrap_or(0);
            println!(
                "\n{} synced rebuyengine → {} ({} paths)",
                "✓".green(),
                out_path.display(),
                path_count,
            );
            Ok(())
        }
        other => anyhow::bail!("sync not implemented for '{other}'"),
    }
}

fn sync_ci4(label: &str, dirname: &str) -> Result<()> {
    let repo_path = rebuy_repo_path(dirname);
    if !repo_path.exists() {
        anyhow::bail!(
            "{label} not found at {} — set REBUY_ROOT or clone the repo",
            repo_path.display()
        );
    }
    print!("  scanning {label} routes and schemas");
    let spec = scanner::ci4_generator::generate(&repo_path)?;
    let out_path = write_spec(label, &spec)?;
    let path_count = spec["paths"].as_object().map(|p| p.len()).unwrap_or(0);
    let schema_count = spec["components"]["schemas"]
        .as_object()
        .map(|s| s.len())
        .unwrap_or(0);
    println!(
        "\n{} synced {label} → {} ({} paths, {} schemas)",
        "✓".green(),
        out_path.display(),
        path_count,
        schema_count,
    );
    Ok(())
}
