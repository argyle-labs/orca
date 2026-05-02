use std::sync::Arc;

use axum::{http::StatusCode, response::{IntoResponse, Json, Response}};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use super::mcp_client::McpPool;

pub type McpState = Arc<McpPool>;

pub fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(ErrorResponse { error: msg.to_string() })).into_response()
}

/// Run a DB closure and return the result as JSON, or a 500 on error.
pub fn db_json<T, F>(f: F) -> Response
where
    T: serde::Serialize,
    F: FnOnce() -> anyhow::Result<T>,
{
    match f() {
        Ok(val) => Json(val).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Run a DB closure that returns `()` and respond with `{ ok: true }`, or 500.
pub fn db_ok<F>(f: F) -> Response
where
    F: FnOnce() -> anyhow::Result<()>,
{
    match f() {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Run a DB remove closure (returns `bool` = found) and respond with ok/404/500.
pub fn db_remove<F>(kind: &str, name: &str, f: F) -> Response
where
    F: FnOnce() -> anyhow::Result<bool>,
{
    match f() {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, &format!("{kind} '{name}' not found")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── Shared response schemas ───────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

pub use crate::serve::tree::{TreeNode, NodeType};

#[derive(Serialize, ToSchema)]
pub struct SearchResult {
    pub root: String,
    pub path: String,
    pub matches: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct McpRunRequest {
    pub server: String,
    pub name: String,
    pub arguments: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub struct McpRunResponse {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

#[derive(Serialize, ToSchema)]
pub struct DockerService {
    pub name: String,
    pub state: String,
    pub running: bool,
    pub health: String,
    pub ports: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DockerServicesResponse {
    #[serde(rename = "composeFile")]
    pub compose_file: Option<String>,
    pub services: Vec<DockerService>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct DockerActionRequest {
    #[serde(rename = "projectPath")]
    pub project_path: String,
    pub service: Option<String>,
    pub action: String,
    pub tail: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub struct DockerActionResponse {
    pub output: String,
    #[serde(rename = "composeFile")]
    pub compose_file: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Ctx7Response {
    #[serde(rename = "libraryId")]
    pub library_id: String,
    pub title: String,
    pub topic: Option<String>,
    pub content: String,
}

#[derive(Serialize, ToSchema)]
pub struct SchemaResponse {
    pub tabs: Vec<SchemaTab>,
    #[serde(rename = "showTabs")]
    pub show_tabs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

#[derive(Serialize, ToSchema)]
pub struct SchemaTab {
    pub title: String,
    pub tables: Vec<Value>,
    pub columns: Value,
    #[serde(rename = "foreignKeys")]
    pub foreign_keys: Vec<Value>,
    pub domains: Value,
}

#[derive(Serialize, ToSchema)]
pub struct HealthCheck {
    pub label: String,
    pub tool: String,
    pub output: String,
    pub ok: bool,
}

#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    pub timestamp: String,
    pub checks: Vec<HealthCheck>,
}

#[derive(Serialize, ToSchema)]
pub struct LogService {
    pub name: String,
    pub state: String,
    pub running: bool,
    pub health: String,
    pub ports: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct LogProject {
    pub project: String,
    pub path: String,
    pub services: Vec<LogService>,
}

#[derive(Serialize, ToSchema)]
pub struct LogServicesResponse {
    pub projects: Vec<LogProject>,
}

#[derive(Serialize, ToSchema)]
pub struct LogsResponse {
    pub output: String,
}

#[derive(Serialize, ToSchema)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub enabled: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct McpServerAddRequest {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Deserialize, ToSchema)]
pub struct TestRunQuery {
    /// Which suite to run: rust | frontend | e2e | all
    pub suite: String,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct DockerRuntimeInfo {
    pub name: String,
    #[serde(rename = "socketPath", skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// HTTP URL for web-based orchestrators (Dockge, Portainer)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub enabled: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct DockerRuntimeAddRequest {
    pub name: String,
    #[serde(rename = "socketPath")]
    pub socket_path: Option<String>,
    pub host: Option<String>,
    /// HTTP URL for web-based orchestrators (Dockge, Portainer)
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct SchemaDbInfo {
    pub name: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: String,
    pub database: String,
    pub container: Option<String>,
    #[serde(rename = "domainsFile", skip_serializing_if = "Option::is_none")]
    pub domains_file: Option<String>,
    pub enabled: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct SchemaDbAddRequest {
    pub name: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
    pub container: Option<String>,
    #[serde(rename = "domainsFile")]
    pub domains_file: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct SpecRegisterRequest {
    pub name: String,
    pub url: String,
}

#[derive(Serialize, ToSchema)]
pub struct SpecInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "sourceMcp", skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(rename = "pathCount", skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    #[serde(rename = "cachedAt", skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, ToSchema)]
pub struct TestRunResponse {
    pub suite: String,
    pub output: String,
    pub exit_code: i32,
    pub passed: u32,
    pub failed: u32,
    pub duration_ms: u64,
}

// ── Handler prelude ───────────────────────────────────────────────────────────
// Import this with `use super::prelude::*;` in every handler module.
// All types listed here are available unqualified — critical for utoipa body
// annotations, which use the literal token path as the $ref name. Never write
// `super::SomeType` inside a utoipa macro; always import it via this prelude.
pub(super) mod prelude {
    #[allow(unused_imports)]
    pub use super::{DockerRuntimeAddRequest, DockerRuntimeInfo, ErrorResponse, McpServerAddRequest, McpServerInfo, McpState, OkResponse, SchemaDbAddRequest, SchemaDbInfo, SpecInfo, SpecRegisterRequest, db_json, db_ok, db_remove, err};
}

// ── Sub-modules ───────────────────────────────────────────────────────────────

pub mod atlassian;
pub mod download;
pub mod pdf;
pub mod bitbucket;
pub mod ctx7;
pub mod docker;
pub mod docs;
pub mod health;
pub mod learning;
pub mod logs;
pub mod mcp;
pub mod mcp_mappings;
pub mod schema;
pub mod docker_registry;
pub mod schema_registry;
pub mod specs;
pub mod system;
pub mod tests_handler;

pub use atlassian::*;
pub use download::*;
pub use pdf::*;
pub use bitbucket::*;
pub use ctx7::*;
pub use docker::*;
pub use docs::*;
pub use health::*;
pub use learning::*;
pub use logs::*;
pub use mcp::*;
pub use mcp_mappings::*;
pub use schema::*;
pub use docker_registry::*;
pub use schema_registry::*;
pub use specs::*;
pub use system::*;
pub use tests_handler::*;
