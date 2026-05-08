use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Scan a Next.js App Router project for `route.ts`/`route.tsx`/`route.js` files
/// and produce an OpenAPI 3.1 spec.
///
/// We look for:
///   <repo>/apps/nextjs/app/**/route.{ts,tsx,js}        (monorepo layout)
///   <repo>/app/**/route.{ts,tsx,js}                    (single-app layout)
///   <repo>/src/app/**/route.{ts,tsx,js}                (src/ layout)
///
/// The first directory that exists wins. Each route file's path becomes the
/// API path (relative to the chosen `app/` root), and we look for top-level
/// `export const GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS` declarations.
pub fn generate(repo_path: &Path) -> Result<Value> {
    let app_dir = locate_app_dir(repo_path)?;
    let mut paths = serde_json::Map::new();

    walk(&app_dir, &app_dir, &mut paths)?;

    let spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "admin-nextjs",
            "version": "0.0.0",
            "description": "Auto-generated from Next.js App Router route handlers"
        },
        "x-orca": {
            "repo": "admin-nextjs",
            "project": "rebuy",
            "source": "scanned",
            "scanner": "nextjs",
            "appRoot": app_dir.display().to_string(),
            "capturedAt": chrono::Utc::now().to_rfc3339()
        },
        "paths": Value::Object(paths),
        "components": { "schemas": {} }
    });
    Ok(spec)
}

fn locate_app_dir(repo: &Path) -> Result<PathBuf> {
    let candidates = [
        repo.join("apps/nextjs/app"),
        repo.join("app"),
        repo.join("src/app"),
    ];
    for c in &candidates {
        if c.is_dir() {
            return Ok(c.clone());
        }
    }
    anyhow::bail!(
        "no Next.js app/ directory found under {} (tried apps/nextjs/app, app, src/app)",
        repo.display()
    )
}

fn walk(root: &Path, dir: &Path, paths: &mut serde_json::Map<String, Value>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip node_modules, .next, tests, etc.
        if p.is_dir() {
            if name == "node_modules" || name == ".next" || name.starts_with('_') {
                continue;
            }
            walk(root, &p, paths)?;
        } else if matches!(
            p.file_name().and_then(|s| s.to_str()),
            Some("route.ts" | "route.tsx" | "route.js")
        ) && let Some((api_path, ops)) = scan_route_file(root, &p)?
        {
            paths.insert(api_path, ops);
        }
    }
    Ok(())
}

fn scan_route_file(root: &Path, file: &Path) -> Result<Option<(String, Value)>> {
    let rel = file
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    // Convert "discounts" / "smartcart/combined" → "/discounts" / "/smartcart/combined".
    // Convert "[id]" segments → "{id}".
    let segments: Vec<String> = rel
        .components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().to_string();
            // Strip Next.js route groups: "(group)" segments don't appear in URL.
            if s.starts_with('(') && s.ends_with(')') {
                String::new()
            } else if let Some(name) = s.strip_prefix("[...").and_then(|s| s.strip_suffix(']')) {
                format!("{{{name}}}")
            } else if let Some(name) = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                format!("{{{name}}}")
            } else {
                s
            }
        })
        .filter(|s| !s.is_empty())
        .collect();

    let api_path = format!("/{}", segments.join("/"));

    let src = std::fs::read_to_string(file)?;
    let methods = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    let mut ops = serde_json::Map::new();
    for m in methods {
        // Match: export const GET = , export async function GET(, export { GET }
        let patterns = [
            format!("export const {m}"),
            format!("export async function {m}"),
            format!("export function {m}"),
        ];
        if patterns.iter().any(|p| src.contains(p.as_str())) {
            ops.insert(
                m.to_lowercase(),
                json!({
                    "summary": format!("{m} {api_path}"),
                    "tags": ["admin-nextjs"],
                    "responses": {
                        "200": { "description": "OK" }
                    },
                    "x-orca-source": file
                        .strip_prefix(root.parent().unwrap_or(root))
                        .unwrap_or(file)
                        .display()
                        .to_string()
                }),
            );
        }
    }

    if ops.is_empty() {
        Ok(None)
    } else {
        Ok(Some((api_path, Value::Object(ops))))
    }
}
