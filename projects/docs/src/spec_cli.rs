// CLI spec command passing through spec/config blobs; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use db;
use scanner;

#[derive(Subcommand)]
pub enum SpecAction {
    /// List all registered external specs (disk + orca.db)
    List,
    /// Register a repo and scaffold a spec file if one doesn't exist
    Add {
        /// Repository name (e.g. admin-api)
        repo: String,
        /// Project the repo belongs to
        #[arg(long)]
        project: String,
        /// Base URL for the API (e.g. https://api.example.com)
        #[arg(long)]
        url: Option<String>,
        /// Short description
        #[arg(long)]
        description: Option<String>,
    },
    /// Fetch an OpenAPI spec from a URL and store it in orca.db
    Register {
        /// Display name for the spec
        name: String,
        /// URL to fetch the OpenAPI JSON from
        #[arg(long)]
        url: String,
    },
    /// Re-fetch URL-registered specs from their source URLs
    Refresh {
        /// Spec name to refresh. Omit with --all to refresh every URL-based spec.
        name: Option<String>,
        /// Refresh all URL-registered specs
        #[arg(long)]
        all: bool,
    },
    /// Remove a URL-registered spec from orca.db
    Unregister { name: String },
    /// Dump orca's own OpenAPI spec to stdout (no server required — used by `make build`)
    Dump,
}

pub fn cmd_spec(action: SpecAction) -> Result<()> {
    match action {
        SpecAction::List => {
            let registry = scanner::SpecRegistry::load()?;
            let conn = db::open_default()?;
            let db_specs = db::openapi_specs::list(&conn)?;

            if !registry.entries.is_empty() {
                println!("{}", "Disk specs:".green());
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

            if !db_specs.is_empty() {
                if !registry.entries.is_empty() {
                    println!();
                }
                println!("{}", "URL-registered specs:".green());
                for s in &db_specs {
                    let url = s.url.as_deref().unwrap_or("-");
                    let cached = s.cached_at.as_deref().unwrap_or("-");
                    println!(
                        "  {}  url={}  cached={}",
                        s.name.cyan(),
                        url.dimmed(),
                        cached.dimmed(),
                    );
                }
            }

            if registry.entries.is_empty() && db_specs.is_empty() {
                println!(
                    "{}",
                    "no specs registered — use `orca spec add <repo>` or `orca spec register --url <url> <name>`".dimmed()
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
                "  edit the scaffolded spec manually, then restart `orca serve`".dimmed()
            );
        }

        SpecAction::Register { name, url } => {
            let conn = db::open_default()?;
            print!("  fetching {}", url.dimmed());
            let resp =
                reqwest::blocking::get(&url).map_err(|e| anyhow::anyhow!("fetch failed: {e}"))?;
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}: {}", resp.status(), url);
            }
            let spec_json: serde_json::Value = resp
                .json()
                .map_err(|e| anyhow::anyhow!("response is not valid JSON: {e}"))?;
            let spec_text = serde_json::to_string(&spec_json)?;
            let row = db::openapi_specs::OpenApiSpecRow {
                name: name.clone(),
                url: Some(url.clone()),
                source_mcp: None,
                spec_json: Some(spec_text),
                cached_at: Some(chrono::Utc::now().to_rfc3339()),
                enabled: true,
            };
            db::openapi_specs::upsert(&conn, &row)?;
            let path_count = spec_json["paths"].as_object().map(|p| p.len()).unwrap_or(0);
            println!(
                "\n{} registered {} from {} ({} paths)",
                "✓".green(),
                name.cyan(),
                url.dimmed(),
                path_count,
            );
        }

        SpecAction::Refresh { name, all } => {
            let conn = db::open_default()?;
            let db_specs = db::openapi_specs::list(&conn)?;
            let to_refresh: Vec<db::openapi_specs::OpenApiSpecRow> = if all {
                db_specs.into_iter().filter(|s| s.url.is_some()).collect()
            } else {
                match name {
                    Some(n) => {
                        let s = db_specs
                            .into_iter()
                            .find(|s| s.name == n)
                            .ok_or_else(|| anyhow::anyhow!("no spec named '{n}'"))?;
                        if s.url.is_none() {
                            anyhow::bail!("spec '{}' has no URL — cannot refresh", s.name);
                        }
                        vec![s]
                    }
                    None => anyhow::bail!("usage: orca spec refresh <name> | --all"),
                }
            };
            if to_refresh.is_empty() {
                println!("{}", "no URL-registered specs to refresh".dimmed());
                return Ok(());
            }
            for spec in to_refresh {
                let url = spec
                    .url
                    .as_ref()
                    .expect("pre-filtered list guarantees URL is present");
                print!("  refreshing {}", spec.name.cyan());
                let resp = match reqwest::blocking::get(url) {
                    Ok(r) => r,
                    Err(e) => {
                        println!("\n{} {}: fetch failed: {}", "✗".red(), spec.name, e);
                        continue;
                    }
                };
                if !resp.status().is_success() {
                    println!("\n{} {}: HTTP {}", "✗".red(), spec.name, resp.status());
                    continue;
                }
                let spec_json: serde_json::Value = match resp.json() {
                    Ok(v) => v,
                    Err(e) => {
                        println!("\n{} {}: invalid JSON: {}", "✗".red(), spec.name, e);
                        continue;
                    }
                };
                let spec_text = serde_json::to_string(&spec_json)?;
                let row = db::openapi_specs::OpenApiSpecRow {
                    name: spec.name.clone(),
                    url: spec.url.clone(),
                    source_mcp: spec.source_mcp.clone(),
                    spec_json: Some(spec_text),
                    cached_at: Some(chrono::Utc::now().to_rfc3339()),
                    enabled: spec.enabled,
                };
                db::openapi_specs::upsert(&conn, &row)?;
                let path_count = spec_json["paths"].as_object().map(|p| p.len()).unwrap_or(0);
                println!(
                    "\n{} refreshed {} ({} paths)",
                    "✓".green(),
                    spec.name.cyan(),
                    path_count,
                );
            }
        }

        SpecAction::Unregister { name } => {
            let conn = db::open_default()?;
            if db::openapi_specs::remove(&conn, &name)? {
                println!("{} unregistered {}", "✓".green(), name.cyan());
            } else {
                println!("{}", format!("no spec named '{name}'").dimmed());
            }
        }

        SpecAction::Dump => {
            // Handled by the server binary (main.rs) which has access to serve::openapi.
            // Should not reach here.
            anyhow::bail!("spec dump must be dispatched from main.rs")
        }
    }
    Ok(())
}
