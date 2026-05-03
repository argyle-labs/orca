use anyhow::Result;
use serde_json::Value;

pub fn spec_dir() -> std::path::PathBuf {
    brain_scanner::openapi_dir()
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
    if let Ok(conn) = brain_utils::db::open_default() {
        if let Ok(db_specs) = brain_utils::db::list_openapi_specs(&conn) {
            if !db_specs.is_empty() {
                if !disk_entries.is_empty() { lines.push(String::new()); }
                lines.push("URL-registered specs:".to_string());
                for s in db_specs {
                    let url = s.url.as_deref().unwrap_or("-");
                    let cached = s.cached_at.as_deref().unwrap_or("-");
                    let paths = s.spec_json.as_deref()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .and_then(|v| v["paths"].as_object().map(|p| p.len()))
                        .unwrap_or(0);
                    lines.push(format!("• {} ({} paths)  url={}  cached={}", s.name, paths, url, cached));
                }
            }
        }
    }

    if lines.len() == 1 {
        lines.push("no specs registered — use `orca spec add <repo>` or `orca spec register`".to_string());
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
    if let Ok(conn) = brain_utils::db::open_default() {
        if let Ok(Some(row)) = brain_utils::db::get_openapi_spec(&conn, repo) {
            if let Some(raw) = row.spec_json {
                return Ok(raw);
            }
        }
    }
    anyhow::bail!("no spec for '{repo}' — check ~/orca/openapi/{repo}.json or run `orca spec register`")
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
        anyhow::anyhow!("no public spec for '{repo}' — check ~/orca/openapi/{repo}.public.json")
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
        anyhow::anyhow!("no GraphQL schema for '{repo}' — check ~/orca/openapi/{repo}.graphql")
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
        anyhow::anyhow!("no GraphQL schema for '{repo}' — check ~/orca/openapi/{repo}.graphql")
    })?;
    let info = brain_scanner::parse_graphql_sdl(repo, &sdl)?;
    Ok(serde_json::to_string_pretty(&info)?)
}

// ── DB-backed spec management ─────────────────────────────────────────────────

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

pub async fn spec_register(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let url  = args["url"].as_str().ok_or_else(|| anyhow::anyhow!("url is required"))?;

    let spec_json = fetch_spec_json(url).await?;
    let spec_text = serde_json::to_string(&spec_json)?;
    let path_count = spec_json["paths"].as_object().map(|p| p.len()).unwrap_or(0);

    let conn = brain_utils::db::open_default()?;
    let row = brain_utils::db::OpenApiSpecRow {
        name: name.to_string(),
        url: Some(url.to_string()),
        source_mcp: None,
        spec_json: Some(spec_text),
        cached_at: Some(chrono::Utc::now().to_rfc3339()),
        enabled: true,
    };
    brain_utils::db::upsert_openapi_spec(&conn, &row)?;
    Ok(format!("registered '{name}' from {url} ({path_count} paths)"))
}

pub async fn spec_refresh(args: &Value) -> Result<String> {
    let all  = args["all"].as_bool().unwrap_or(false);
    let name = args["name"].as_str();

    let conn = brain_utils::db::open_default()?;
    let db_specs = brain_utils::db::list_openapi_specs(&conn)?;

    let to_refresh: Vec<brain_utils::db::OpenApiSpecRow> = if all {
        db_specs.into_iter().filter(|s| s.url.is_some()).collect()
    } else {
        match name {
            Some(n) => {
                let s = db_specs.into_iter().find(|s| s.name == n)
                    .ok_or_else(|| anyhow::anyhow!("no spec named '{n}'"))?;
                if s.url.is_none() { anyhow::bail!("spec '{n}' has no URL — cannot refresh"); }
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
        let url = spec.url.as_ref().unwrap().clone();
        match fetch_spec_json(&url).await {
            Err(e) => { results.push(format!("✗ {}: {e}", spec.name)); continue; }
            Ok(spec_json) => {
                let path_count = spec_json["paths"].as_object().map(|p| p.len()).unwrap_or(0);
                let spec_text = serde_json::to_string(&spec_json)?;
                let row = brain_utils::db::OpenApiSpecRow {
                    name: spec.name.clone(),
                    url: Some(url),
                    source_mcp: spec.source_mcp.clone(),
                    spec_json: Some(spec_text),
                    cached_at: Some(chrono::Utc::now().to_rfc3339()),
                    enabled: spec.enabled,
                };
                brain_utils::db::upsert_openapi_spec(&conn, &row)?;
                results.push(format!("✓ {} ({path_count} paths)", spec.name));
            }
        }
    }
    Ok(results.join("\n"))
}

pub fn spec_unregister(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let conn = brain_utils::db::open_default()?;
    if brain_utils::db::remove_openapi_spec(&conn, name)? {
        Ok(format!("unregistered '{name}'"))
    } else {
        anyhow::bail!("no spec named '{name}'")
    }
}
