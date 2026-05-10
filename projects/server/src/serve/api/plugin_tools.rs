//! HTTP surface for plugin-declared tools.
//!
//! `mcp-serve` runs in a separate process from the orca daemon, so it cannot
//! reach the in-process `PluginRegistry` directly. The MCP bridge forwards
//! plugin `tools/call` invocations through these endpoints; the daemon then
//! dispatches via `plugin_host::global()`.

use std::time::Duration;

use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use super::prelude::*;

/// Default per-call timeout when the caller doesn't override it.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, ToSchema)]
pub struct PluginToolInfo {
    #[serde(rename = "pluginId")]
    pub plugin_id: String,
    pub name: String,
    #[serde(rename = "fqName")]
    pub fq_name: String,
    pub description: String,
    /// JSON Schema for the tool's input arguments. Returned as a parsed
    /// object so the MCP layer can pass it straight through.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub sensitivity: String,
    /// Whether the owning plugin is currently connected to the host.
    pub connected: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct PluginToolCallRequest {
    /// Arbitrary JSON the plugin tool expects. Validated by the plugin, not
    /// by the host — the host is a transparent forwarder.
    pub arguments: Value,
    /// Optional override (seconds). Defaults to `DEFAULT_CALL_TIMEOUT`.
    #[serde(default, rename = "timeoutSecs")]
    pub timeout_secs: Option<u64>,
}

#[derive(Serialize, ToSchema)]
pub struct PluginToolCallResponse {
    /// Opaque payload returned by the plugin tool.
    pub result: Value,
}

// ── GET /api/plugin-tools ─────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/plugin-tools",
    operation_id = "listPluginTools",
    responses(
        (status = 200, description = "All declared plugin tools", body = Vec<PluginToolInfo>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugin-tools"
)]
pub async fn plugin_tools_list_handler() -> Response {
    let registry = crate::plugin_host::global();
    let connected: std::collections::HashSet<String> = registry
        .as_ref()
        .map(|r| r.connected_ids().into_iter().collect())
        .unwrap_or_default();

    db_json(|| {
        let conn = db::open_default()?;
        let rows = db::plugin_tools::list_all(&conn)?;
        let infos = rows
            .into_iter()
            .map(|r| {
                let schema = serde_json::from_str(&r.input_schema).unwrap_or(Value::Null);
                PluginToolInfo {
                    connected: connected.contains(&r.plugin_id),
                    plugin_id: r.plugin_id,
                    name: r.name,
                    fq_name: r.fq_name,
                    description: r.description,
                    input_schema: schema,
                    sensitivity: r.sensitivity,
                }
            })
            .collect::<Vec<_>>();
        Ok(infos)
    })
}

// ── POST /api/plugin-tools/{fq_name}/call ────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/plugin-tools/{fq_name}/call",
    operation_id = "callPluginTool",
    params(("fq_name" = String, Path, description = "Fully-qualified tool name `<plugin_id>.<name>`")),
    request_body = PluginToolCallRequest,
    responses(
        (status = 200, description = "Tool result", body = PluginToolCallResponse),
        (status = 404, description = "Tool not registered or plugin not connected", body = ErrorResponse),
        (status = 502, description = "Plugin returned an error or did not respond", body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "plugin-tools"
)]
pub async fn plugin_tool_call_handler(
    Path(fq_name): Path<String>,
    Json(req): Json<PluginToolCallRequest>,
) -> Response {
    // 1. Resolve fq_name → (plugin_id, tool_name) via DB.
    let row = match (|| -> anyhow::Result<Option<db::plugin_tools::PluginToolRow>> {
        let conn = db::open_default()?;
        db::plugin_tools::get(&conn, &fq_name)
    })() {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err(
                StatusCode::NOT_FOUND,
                &format!("plugin tool '{fq_name}' is not declared"),
            );
        }
        Err(e) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    };

    // 2. Look up the connected plugin.
    let registry = match crate::plugin_host::global() {
        Some(r) => r,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "plugin host not running — orca was started without the plugin host",
            );
        }
    };
    let handle = match registry.get(&row.plugin_id) {
        Some(h) => h,
        None => {
            return err(
                StatusCode::NOT_FOUND,
                &format!(
                    "plugin '{}' is declared but not currently connected",
                    row.plugin_id
                ),
            );
        }
    };

    let timeout = req
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CALL_TIMEOUT);

    // 3. Forward to the plugin and surface the opaque result.
    match handle.call_tool(&row.name, req.arguments, timeout).await {
        Ok(result) => Json(PluginToolCallResponse { result }).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}
