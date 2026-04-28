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
    let path = spec_dir().join("registry.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("registry.json not found at {}", path.display()))?;
    let entries: Vec<Value> = serde_json::from_str(&raw)?;
    let mut lines = vec!["Rebuy OpenAPI Spec Registry\n".to_string()];
    for entry in &entries {
        let repo = entry["repo"].as_str().unwrap_or("?");
        let desc = entry["description"].as_str().unwrap_or("");
        let paths = entry["pathCount"].as_u64().unwrap_or(0);
        let has_public = entry["files"]["public"].is_string();
        let has_graphql = spec_dir().join(format!("{repo}.graphql")).exists();
        lines.push(format!(
            "• {repo} ({paths} paths){}{}\n  {desc}",
            if has_public { " [public]" } else { "" },
            if has_graphql { " [graphql]" } else { "" },
        ));
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
    let path = spec_dir().join(format!("{repo}.json"));
    std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("no spec for '{repo}' — check ~/brain/openapi/{repo}.json"))
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
        anyhow::anyhow!("no public spec for '{repo}' — check ~/brain/openapi/{repo}.public.json")
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
        anyhow::anyhow!("no GraphQL schema for '{repo}' — check ~/brain/openapi/{repo}.graphql")
    })
}
