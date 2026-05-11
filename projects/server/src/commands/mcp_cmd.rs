//! MCP server discovery helper — `mcp_sync_server` shared by the management
//! service impl (orca mcp sync) and the `/api/mcp/sync` REST handler.
//!
//! `serde_json::Value` is allowed here because we're parsing wire-level MCP
//! protocol responses from a remote server we don't control — modelling each
//! possible MCP `tools/list` shape with typed structs would just be a lossy
//! re-encoding of the same opaque blob.
#![allow(clippy::disallowed_types)]

use anyhow::Context;
use db::mcp_servers::ServerRow;

pub fn mcp_sync_server(server: &ServerRow, _threshold: f64) -> anyhow::Result<(usize, usize)> {
    let conn = db::open_default()?;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(&server.command)
        .args(&server.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", server.command))?;

    let mut stdin = child
        .stdin
        .take()
        .context("MCP child process missing stdin pipe")?;
    let stdout = child
        .stdout
        .take()
        .context("MCP child process missing stdout pipe")?;
    let mut reader = BufReader::new(stdout);

    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": { "name": "orca-sync", "version": "0.1.0" } }
    });
    writeln!(stdin, "{}", init)?;
    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )?;

    let tools_req =
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    writeln!(stdin, "{}", tools_req)?;
    stdin.flush()?;

    let mut external_tools: Vec<serde_json::Value> = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
            && v["id"] == 2
        {
            if let Some(arr) = v["result"]["tools"].as_array() {
                external_tools = arr.clone();
            }
            break;
        }
        line.clear();
    }
    let _ = child.kill();

    if external_tools.is_empty() {
        anyhow::bail!("no tools returned from {}", server.name);
    }

    let existing = db::tool_mappings::list(&conn, &server.name)?;
    let already_mapped: std::collections::HashSet<String> = existing
        .iter()
        .filter(|r| r.match_type == "explicit")
        .map(|r| r.orca_tool.clone())
        .collect();

    let mut added = 0usize;
    let mut skipped = 0usize;
    for tool in &external_tools {
        let ext_name = match tool["name"].as_str() {
            Some(n) => n,
            None => continue,
        };
        if already_mapped.contains(ext_name) {
            skipped += 1;
            continue;
        }
        if let Ok(Some(_)) = db::tool_mappings::lookup(&conn, ext_name) {
            skipped += 1;
            continue;
        }
        let row = db::tool_mappings::MappingRow {
            orca_tool: ext_name.to_string(),
            mcp_name: server.name.clone(),
            external_tool: ext_name.to_string(),
            match_type: "auto_discovered".to_string(),
            confidence: Some(1.0),
            enabled: true,
        };
        db::tool_mappings::upsert(&conn, &row)?;
        added += 1;
    }
    Ok((added, skipped))
}
