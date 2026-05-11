#![allow(clippy::disallowed_types)] // OpenAPI spec serving — dynamic JSON construction for spec blobs
use anyhow::Result;
use serde_json::Value;

pub fn spec_dir() -> std::path::PathBuf {
    crate::scanner::specs_dir()
}

pub fn validate_spec_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub fn list_rebuy_specs() -> Result<String> {
    let dir = spec_dir();
    let mut lines = vec!["OpenAPI Spec Registry\n".to_string()];

    // Disk-scanned specs (registry.json)
    let registry_path = dir.join("registry.json");
    let disk_entries: Vec<Value> = std::fs::read_to_string(&registry_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    if !disk_entries.is_empty() {
        lines.push("Disk specs:".to_string());
        for entry in &disk_entries {
            let repo = entry["repo"].as_str().unwrap_or("?");
            let desc = entry["description"].as_str().unwrap_or("");
            let paths = entry["pathCount"].as_u64().unwrap_or(0);
            let has_public = entry["files"]["public"].is_string();
            let has_graphql = dir.join(format!("{repo}.graphql")).exists();
            lines.push(format!(
                "• {repo} ({paths} paths){}{}\n  {desc}",
                if has_public { " [public]" } else { "" },
                if has_graphql { " [graphql]" } else { "" },
            ));
        }
    }

    // DB-registered specs (URL-fetched)
    if let Ok(conn) = db::open_default()
        && let Ok(db_specs) = db::openapi_specs::list(&conn)
        && !db_specs.is_empty()
    {
        if !disk_entries.is_empty() {
            lines.push(String::new());
        }
        lines.push("URL-registered specs:".to_string());
        for s in db_specs {
            let url = s.url.as_deref().unwrap_or("-");
            let cached = s.cached_at.as_deref().unwrap_or("-");
            let paths = s
                .spec_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|v| v["paths"].as_object().map(|p| p.len()))
                .unwrap_or(0);
            lines.push(format!(
                "• {} ({} paths)  url={}  cached={}",
                s.name, paths, url, cached
            ));
        }
    }

    if lines.len() == 1 {
        lines.push(
            "no specs registered — use `orca spec add <repo>` or `orca spec register`".to_string(),
        );
    }

    Ok(lines.join("\n"))
}

pub fn get_rebuy_spec(args: &Value) -> Result<String> {
    let repo = args["repo"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("repo is required"))?;
    if !validate_spec_repo(repo) {
        anyhow::bail!("invalid repo name");
    }
    // Disk first, then DB-cached
    let path = spec_dir().join(format!("{repo}.json"));
    if let Ok(raw) = std::fs::read_to_string(&path) {
        return Ok(raw);
    }
    if let Ok(conn) = db::open_default()
        && let Ok(Some(row)) = db::openapi_specs::get(&conn, repo)
        && let Some(raw) = row.spec_json
    {
        return Ok(raw);
    }
    anyhow::bail!(
        "no spec for '{repo}' — check ~/.orca/openapi/{repo}.json or run `orca spec register`"
    )
}

pub fn get_rebuy_spec_public(args: &Value) -> Result<String> {
    let repo = args["repo"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("repo is required"))?;
    if !validate_spec_repo(repo) {
        anyhow::bail!("invalid repo name");
    }
    let path = spec_dir().join(format!("{repo}.public.json"));
    std::fs::read_to_string(&path).map_err(|_| {
        anyhow::anyhow!("no public spec for '{repo}' — check ~/.orca/openapi/{repo}.public.json")
    })
}

pub fn get_rebuy_graphql_schema(args: &Value) -> Result<String> {
    let repo = args["repo"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("repo is required"))?;
    if !validate_spec_repo(repo) {
        anyhow::bail!("invalid repo name");
    }
    let path = spec_dir().join(format!("{repo}.graphql"));
    std::fs::read_to_string(&path).map_err(|_| {
        anyhow::anyhow!("no GraphQL schema for '{repo}' — check ~/.orca/openapi/{repo}.graphql")
    })
}

pub fn get_graphql_info(args: &Value) -> Result<String> {
    let repo = args["repo"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("repo is required"))?;
    if !validate_spec_repo(repo) {
        anyhow::bail!("invalid repo name");
    }
    let path = spec_dir().join(format!("{repo}.graphql"));
    let sdl = std::fs::read_to_string(&path).map_err(|_| {
        anyhow::anyhow!("no GraphQL schema for '{repo}' — check ~/.orca/openapi/{repo}.graphql")
    })?;
    let info = crate::scanner::parse_graphql_sdl(repo, &sdl)?;
    Ok(serde_json::to_string_pretty(&info)?)
}

// ── DB-backed spec management — superseded by spec_registry_service.rs ────────
#[allow(dead_code)]
async fn fetch_spec_json(url: &str) -> Result<Value> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| anyhow::anyhow!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}: {}", resp.status(), url);
    }
    resp.json::<Value>()
        .await
        .map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))
}

#[allow(dead_code)]
pub async fn spec_register(args: &Value) -> Result<String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("url is required"))?;

    let spec_json = fetch_spec_json(url).await?;
    let spec_text = serde_json::to_string(&spec_json)?;
    let path_count = spec_json["paths"].as_object().map(|p| p.len()).unwrap_or(0);

    let conn = db::open_default()?;
    let row = db::openapi_specs::OpenApiSpecRow {
        name: name.to_string(),
        url: Some(url.to_string()),
        source_mcp: None,
        spec_json: Some(spec_text),
        cached_at: Some(chrono::Utc::now().to_rfc3339()),
        enabled: true,
    };
    db::openapi_specs::upsert(&conn, &row)?;
    Ok(format!(
        "registered '{name}' from {url} ({path_count} paths)"
    ))
}

#[allow(dead_code)]
pub async fn spec_refresh(args: &Value) -> Result<String> {
    let all = args["all"].as_bool().unwrap_or(false);
    let name = args["name"].as_str();

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
                    anyhow::bail!("spec '{n}' has no URL — cannot refresh");
                }
                vec![s]
            }
            None => anyhow::bail!("name or all=true required"),
        }
    };

    if to_refresh.is_empty() {
        return Ok("no URL-registered specs to refresh".to_string());
    }

    let mut results = Vec::new();
    for spec in to_refresh {
        let url = spec
            .url
            .as_ref()
            .expect("pre-filtered list guarantees URL is present")
            .clone();
        match fetch_spec_json(&url).await {
            Err(e) => {
                results.push(format!("✗ {}: {e}", spec.name));
                continue;
            }
            Ok(spec_json) => {
                let path_count = spec_json["paths"].as_object().map(|p| p.len()).unwrap_or(0);
                let spec_text = serde_json::to_string(&spec_json)?;
                let row = db::openapi_specs::OpenApiSpecRow {
                    name: spec.name.clone(),
                    url: Some(url),
                    source_mcp: spec.source_mcp.clone(),
                    spec_json: Some(spec_text),
                    cached_at: Some(chrono::Utc::now().to_rfc3339()),
                    enabled: spec.enabled,
                };
                db::openapi_specs::upsert(&conn, &row)?;
                results.push(format!("✓ {} ({path_count} paths)", spec.name));
            }
        }
    }
    Ok(results.join("\n"))
}

#[allow(dead_code)]
pub fn spec_unregister(args: &Value) -> Result<String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let conn = db::open_default()?;
    if db::openapi_specs::remove(&conn, name)? {
        Ok(format!("unregistered '{name}'"))
    } else {
        anyhow::bail!("no spec named '{name}'")
    }
}
