/// MCP stdio server — exposes brain tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: brain mcp-serve
/// Register: claude mcp add brain-local -- brain mcp-serve
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::backend::buffer_sink;
use crate::config::Config;
use crate::context::ProjectContext;
use crate::log;
use crate::session::Session;

pub async fn serve(config: &Config) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut out = tokio::io::BufWriter::new(stdout);

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // MCP notifications (no id) are fire-and-forget — replying would break the protocol.
        if req.get("id").is_none() {
            continue;
        }

        let response = match method {
            "initialize" => reply(id, json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "brain", "version": env!("CARGO_PKG_VERSION") }
            })),
            "ping" => reply(id, json!({})),
            "tools/list" => reply(id, json!({ "tools": tool_defs() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];
                let result = dispatch(name, args, config).await;
                match result {
                    Ok(text) => reply(id, json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    })),
                    Err(e) => reply(id, json!({
                        "content": [{ "type": "text", "text": format!("Error: {e}") }],
                        "isError": true
                    })),
                }
            }
            _ => error_reply(id, -32601, &format!("method not found: {method}")),
        };

        let mut payload = serde_json::to_string(&response)?;
        payload.push('\n');
        out.write_all(payload.as_bytes()).await?;
        out.flush().await?;
    }

    Ok(())
}

// ── Tool definitions ──────────────────────────────────────────────────────────

fn tool_defs() -> Value {
    json!([
        {
            "name": "brain_agents",
            "description": "List all available brain agents with their names and descriptions.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "brain_run",
            "description": "Delegate a task to a local brain agent running on the local LLM. Use for tasks that don't need Claude-level reasoning — code explanation, note-taking, file ops, quick lookups. Returns the agent's full response.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Agent name (e.g. wolf, owl, fox, crow, raven, badger)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Task or question to send to the agent"
                    }
                },
                "required": ["agent", "prompt"]
            }
        },
        {
            "name": "brain_search_logs",
            "description": "Search brain session history for a keyword. Returns matching log entries with session ID, role, and content preview.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword to search for across all session logs"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "brain_get_context",
            "description": "Load the memory context for a brain project. Returns MEMORY.md index and all memory files for the project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name (e.g. halvor, rebuy-db, dotfiles)"
                    }
                },
                "required": ["project"]
            }
        },
        {
            "name": "list_roots",
            "description": "List available documentation roots (rebuy, brain) with file counts and paths.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_tree",
            "description": "Get the compacted documentation tree for a root, optionally scoped to a subpath. Returns a JSON tree of .md files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Root name: rebuy | brain" },
                    "path": { "type": "string", "description": "Optional subpath within root (e.g. \"admin-api\" or \"ai/claude/agents\")" }
                },
                "required": ["root"]
            }
        },
        {
            "name": "read_doc",
            "description": "Read a documentation file by root and relative path (e.g. root=rebuy, path=admin-api/README).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Root name: rebuy | brain" },
                    "path": { "type": "string", "description": "Path relative to root, without extension" }
                },
                "required": ["root", "path"]
            }
        },
        {
            "name": "search_docs",
            "description": "Search documentation files for a keyword across one or all roots.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search term (case-insensitive)" },
                    "root": { "type": "string", "description": "Limit to root: rebuy | brain | all (default: all)" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "list_commands",
            "description": "List all Claude slash commands and skills from the brain vault.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "brain_list_services",
            "description": "List all running docker compose services across all rebuy projects. Returns project name, path, and per-service state/health/ports.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "brain_service_logs",
            "description": "Fetch docker compose logs for a running rebuy service. Specify the project path and service name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Absolute path to the project directory (e.g. /Users/scottkey/code/rebuy/admin-api)"
                    },
                    "service": {
                        "type": "string",
                        "description": "Service name as defined in docker-compose (e.g. php, nginx, admin-api-nginx)"
                    },
                    "tail": {
                        "type": "integer",
                        "description": "Number of log lines to return (default: 200)"
                    }
                },
                "required": ["project", "service"]
            }
        },
        {
            "name": "brain_run_tests",
            "description": "Run the brain project test suite. Returns test output with pass/fail counts. Suites: rust (cargo test), frontend (vitest), e2e (playwright), all.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "suite": {
                        "type": "string",
                        "description": "Which suite to run: rust | frontend | e2e | all (default: rust)"
                    }
                }
            }
        }
    ])
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
        "brain_agents" => agents(config),
        "brain_run" => run(args, config).await,
        "brain_search_logs" => search_logs(args, config),
        "brain_get_context" => get_context(args, config),
        "list_roots" => list_roots(config),
        "get_tree" => get_tree(args, config),
        "read_doc" => read_doc(args, config),
        "search_docs" => search_docs(args, config),
        "list_commands" => list_commands(config),
        "brain_list_services" => list_services().await,
        "brain_service_logs" => service_logs(args).await,
        "brain_run_tests" => run_tests(args).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

// ── Tool implementations ──────────────────────────────────────────────────────

fn agents(config: &Config) -> Result<String> {
    let dir = config.agents_dir();
    if !dir.exists() {
        return Ok("No agents directory found.".into());
    }
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut lines = vec!["Available brain agents:".to_string(), String::new()];
    for e in entries {
        let path = e.path();
        let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let desc = frontmatter_field(&path, "description").unwrap_or_default();
        let short: String = desc.chars().take(100).collect();
        let ellipsis = if desc.len() > 100 { "…" } else { "" };
        lines.push(format!("@{name}: {short}{ellipsis}"));
    }
    Ok(lines.join("\n"))
}

async fn run(args: &Value, config: &Config) -> Result<String> {
    let agent = args["agent"].as_str().unwrap_or("wolf");
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;

    let full_prompt = if agent != "wolf" && agent != "brain" {
        format!("Delegate this to @{agent}: {prompt}")
    } else {
        prompt.to_string()
    };

    let (sink, buf) = buffer_sink();
    let ctx = ProjectContext::default();
    let mut session = Session::new_with_output(config.clone(), ctx, sink).await?;
    session.one_shot(full_prompt).await?;

    let bytes = buf.lock().unwrap();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn search_logs(args: &Value, config: &Config) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query is required"))?;

    let matches = log::search_logs(&config.logs_dir(), query, 20)?;
    if matches.is_empty() {
        return Ok(format!("No matches for '{query}'"));
    }

    let mut lines = vec![format!("Found {} match(es) for '{query}':", matches.len()), String::new()];
    for m in &matches {
        let session = m["session"].as_str().unwrap_or("?");
        let role = m["role"].as_str().unwrap_or("?");
        let agent = m["agent"].as_str().unwrap_or("");
        let content = m["content"].as_str().unwrap_or("");
        let preview: String = content.chars().take(200).collect();
        let flag = if m["important"].as_bool() == Some(true) { " ★" } else { "" };
        let prefix = if agent.is_empty() {
            format!("[{role}]")
        } else {
            format!("[{role}/@{agent}]")
        };
        lines.push(format!("{session} {prefix} {preview}{flag}"));
    }
    Ok(lines.join("\n"))
}

fn get_context(args: &Value, config: &Config) -> Result<String> {
    let project = args["project"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("project is required"))?;

    let dir = config.memory_root.join(project);
    if !dir.exists() {
        return Ok(format!("No memory found for project '{project}'"));
    }

    let mut parts = vec![format!("# Memory: {project}"), String::new()];

    let index = dir.join("MEMORY.md");
    if index.exists() {
        parts.push("## MEMORY.md".to_string());
        parts.push(std::fs::read_to_string(&index)?);
    }

    let mut files: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| {
            let p = e.path();
            p.extension().map(|x| x == "md").unwrap_or(false)
                && p.file_name().map(|n| n != "MEMORY.md").unwrap_or(true)
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    for f in files {
        let path = f.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        parts.push(format!("## {name}"));
        parts.push(std::fs::read_to_string(&path)?);
    }

    Ok(parts.join("\n"))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn frontmatter_field(path: &std::path::Path, field: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{field}:");
    content
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).map(|v| v.trim().to_string()))
}

// ── Doc tree helpers ──────────────────────────────────────────────────────────

struct DocRoot {
    name: &'static str,
    path: PathBuf,
    ignored: HashSet<&'static str>,
}

fn doc_roots(config: &Config) -> Vec<DocRoot> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        DocRoot {
            name: "rebuy",
            path: std::env::var("REBUY_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join("code/rebuy")),
            ignored: ["node_modules", ".git", ".next", "dist", "build", "vendor", "www", "docs"]
                .into_iter().collect(),
        },
        DocRoot {
            name: "brain",
            path: config.brain_vault.clone(),
            ignored: [".git", "logs", "memory", "plugins", ".trash", "node_modules"]
                .into_iter().collect(),
        },
    ]
}

fn build_doc_tree(dir: &Path, root_dir: &Path, ignored: &HashSet<&str>) -> Vec<Value> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut nodes: Vec<(bool, String, Value)> = vec![];

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || ignored.contains(name.as_str()) {
            continue;
        }

        let full = entry.path();
        let is_symlink = full.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false);
        let is_dir = if is_symlink {
            full.metadata().map(|m| m.is_dir()).unwrap_or(false)
        } else {
            full.is_dir()
        };

        let rel = full.strip_prefix(root_dir).unwrap_or(&full).to_string_lossy().to_string();

        if is_dir {
            let children = build_doc_tree(&full, root_dir, ignored);
            if !children.is_empty() {
                nodes.push((true, name.clone(), json!({ "name": name, "path": rel, "type": "dir", "children": children })));
            }
        } else {
            let ext = full.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if ext == "md" || ext == "mdx" {
                let stem = full.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(name.clone());
                nodes.push((false, name.clone(), json!({ "name": stem, "path": rel, "type": "file" })));
            }
        }
    }

    nodes.sort_by(|a, b| match (a.0, b.0) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.1.cmp(&b.1),
    });

    nodes.into_iter().map(|(_, _, v)| v).collect()
}

fn count_doc_files(nodes: &[Value]) -> usize {
    nodes.iter().map(|n| {
        if n["type"] == "file" { 1 } else {
            n["children"].as_array().map(|c| count_doc_files(c)).unwrap_or(0)
        }
    }).sum()
}

fn find_single_doc_file(nodes: &[Value]) -> Option<Value> {
    for node in nodes {
        if node["type"] == "file" { return Some(node.clone()); }
        if let Some(children) = node["children"].as_array() {
            if let Some(found) = find_single_doc_file(children) { return Some(found); }
        }
    }
    None
}

fn compact_doc_tree(nodes: Vec<Value>) -> Vec<Value> {
    let mut result = vec![];
    for node in nodes {
        if node["type"] == "file" { result.push(node); continue; }

        let children_raw: Vec<Value> = node["children"].as_array().cloned().unwrap_or_default();
        let children = compact_doc_tree(children_raw);

        if count_doc_files(&children) == 1 {
            if let Some(file) = find_single_doc_file(&children) { result.push(file); continue; }
        }

        if children.len() == 1 && children[0]["type"] == "dir" {
            let child = &children[0];
            let merged = format!("{}/{}", node["name"].as_str().unwrap_or(""), child["name"].as_str().unwrap_or(""));
            let mut n = child.clone();
            n["name"] = json!(merged);
            result.push(n);
            continue;
        }

        let mut n = node.clone();
        n["children"] = json!(children);
        result.push(n);
    }
    result
}

fn collect_all_doc_files(nodes: &[Value]) -> Vec<Value> {
    let mut files = vec![];
    for node in nodes {
        if node["type"] == "file" { files.push(node.clone()); }
        else if let Some(children) = node["children"].as_array() {
            files.extend(collect_all_doc_files(children));
        }
    }
    files
}

fn resolve_doc_file(root_dir: &Path, doc_path: &str) -> Option<PathBuf> {
    for ext in &[".md", ".mdx", ""] {
        let full = root_dir.join(format!("{doc_path}{ext}"));
        if full.is_file() && full.starts_with(root_dir) {
            return Some(full);
        }
    }
    None
}

// ── Doc tool implementations ──────────────────────────────────────────────────

fn list_roots(config: &Config) -> Result<String> {
    let roots = doc_roots(config);
    let mut entries: Vec<Value> = roots.iter().map(|r| {
        let exists = r.path.exists();
        let docs = if exists { count_doc_files(&build_doc_tree(&r.path, &r.path, &r.ignored)) } else { 0 };
        json!({ "root": r.name, "path": r.path.to_string_lossy(), "exists": exists, "docs": docs })
    }).collect();
    entries.push(json!({
        "root": "docs",
        "path": "(embedded in binary)",
        "exists": true,
        "docs": crate::docs::file_count()
    }));
    Ok(serde_json::to_string_pretty(&entries)?)
}

fn get_tree(args: &Value, config: &Config) -> Result<String> {
    let root_name = args["root"].as_str().ok_or_else(|| anyhow::anyhow!("root is required"))?;

    if root_name == "docs" {
        return Ok(serde_json::to_string_pretty(&crate::docs::tree())?);
    }

    let sub_path = args["path"].as_str();
    let roots = doc_roots(config);
    let root = roots.iter().find(|r| r.name == root_name)
        .ok_or_else(|| anyhow::anyhow!("unknown root: {root_name}"))?;

    let dir = sub_path.map(|p| root.path.join(p)).unwrap_or_else(|| root.path.clone());
    let compact = compact_doc_tree(build_doc_tree(&dir, &root.path, &root.ignored));
    Ok(serde_json::to_string_pretty(&compact)?)
}

fn read_doc(args: &Value, config: &Config) -> Result<String> {
    let root_name = args["root"].as_str().ok_or_else(|| anyhow::anyhow!("root is required"))?;
    let doc_path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("path is required"))?;

    if root_name == "docs" {
        return crate::docs::read(doc_path)
            .ok_or_else(|| anyhow::anyhow!("not found: docs/{doc_path}"));
    }

    let roots = doc_roots(config);
    let root = roots.iter().find(|r| r.name == root_name)
        .ok_or_else(|| anyhow::anyhow!("unknown root: {root_name}"))?;

    let full = resolve_doc_file(&root.path, doc_path)
        .ok_or_else(|| anyhow::anyhow!("not found: {root_name}/{doc_path}"))?;

    Ok(std::fs::read_to_string(full)?)
}

fn search_docs(args: &Value, config: &Config) -> Result<String> {
    let query = args["query"].as_str().ok_or_else(|| anyhow::anyhow!("query is required"))?;
    let filter = args["root"].as_str().unwrap_or("all");

    let all_roots = doc_roots(config);
    let roots: Vec<&DocRoot> = all_roots.iter()
        .filter(|r| filter == "all" || r.name == filter)
        .collect();

    let query_lower = query.to_lowercase();
    let mut results: Vec<String> = vec![];

    for root in roots {
        if !root.path.exists() { continue; }
        let files = collect_all_doc_files(&build_doc_tree(&root.path, &root.path, &root.ignored));
        for file in files {
            let rel = file["path"].as_str().unwrap_or("");
            let full = root.path.join(rel);
            let Ok(content) = std::fs::read_to_string(&full) else { continue };
            let matches: Vec<String> = content.lines().enumerate()
                .filter(|(_, l)| l.to_lowercase().contains(&query_lower))
                .take(5)
                .map(|(i, l)| format!("L{}: {}", i + 1, l.trim()))
                .collect();
            if !matches.is_empty() {
                results.push(format!("## {}/{}\n{}", root.name, rel, matches.join("\n")));
            }
        }
    }

    if filter == "all" || filter == "docs" {
        for (path, matches) in crate::docs::search(query) {
            results.push(format!("## docs/{}\n{}", path, matches.join("\n")));
        }
    }

    if results.is_empty() {
        Ok(format!("No results for \"{query}\""))
    } else {
        Ok(results.join("\n\n"))
    }
}

fn list_commands(config: &Config) -> Result<String> {
    let dir = config.brain_vault.join("ai/claude/commands");
    if !dir.exists() {
        return Ok("Commands dir not found.".into());
    }
    let mut files: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| e.file_name());
    let names: Vec<String> = files.iter()
        .map(|e| format!("/{}", e.path().file_stem().unwrap_or_default().to_string_lossy()))
        .collect();
    Ok(names.join("\n"))
}

async fn list_services() -> Result<String> {
    let resp = reqwest::get("http://127.0.0.1:12000/api/logs/services")
        .await?
        .json::<serde_json::Value>()
        .await?;

    let projects = resp["projects"].as_array().cloned().unwrap_or_default();
    if projects.is_empty() {
        return Ok("No projects found.".into());
    }

    let mut lines = vec![];
    for proj in &projects {
        let name = proj["project"].as_str().unwrap_or("?");
        let path = proj["path"].as_str().unwrap_or("?");
        let services = proj["services"].as_array().cloned().unwrap_or_default();
        if services.is_empty() {
            continue;
        }
        lines.push(format!("## {name}  ({path})"));
        for svc in &services {
            let svc_name = svc["name"].as_str().unwrap_or("?");
            let state = svc["state"].as_str().unwrap_or("unknown");
            let health = svc["health"].as_str().unwrap_or("");
            let ports: Vec<&str> = svc["ports"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let health_part = if health.is_empty() { String::new() } else { format!(" [{health}]") };
            let ports_part = if ports.is_empty() { String::new() } else { format!("  ports: {}", ports.join(", ")) };
            lines.push(format!("  {svc_name}: {state}{health_part}{ports_part}"));
        }
    }

    Ok(lines.join("\n"))
}

async fn run_tests(args: &Value) -> Result<String> {
    let suite = args["suite"].as_str().unwrap_or("rust");
    let resp = crate::serve::api::run_test_suite(suite).await?;
    Ok(format!(
        "Suite: {}\nPassed: {} | Failed: {}\nDuration: {}ms\n\n{}",
        resp.suite, resp.passed, resp.failed, resp.duration_ms, resp.output
    ))
}

async fn service_logs(args: &Value) -> Result<String> {
    let project = args["project"].as_str()
        .ok_or_else(|| anyhow::anyhow!("project is required"))?;
    let service = args["service"].as_str()
        .ok_or_else(|| anyhow::anyhow!("service is required"))?;
    let tail = args["tail"].as_u64().unwrap_or(200);

    let tail_str = tail.to_string();
    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:12000/api/logs")
        .query(&[("project", project), ("service", service), ("tail", tail_str.as_str())])
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp["output"].as_str().unwrap_or("(no output)").to_string())
}
