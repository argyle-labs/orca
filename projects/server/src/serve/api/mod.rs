use std::sync::Arc;

use axum::{http::StatusCode, response::{IntoResponse, Json, Response}};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use super::mcp_client::McpPool;

pub type McpState = Arc<McpPool>;

pub fn err(code: StatusCode, msg: &str) -> Response {
    (
        code,
        Json(ErrorResponse {
            error: msg.to_string(),
        }),
    )
        .into_response()
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

#[derive(Deserialize, ToSchema)]
pub struct TestRunQuery {
    /// Which suite to run: rust | frontend | e2e | all
    pub suite: String,
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
    pub use super::{ErrorResponse, McpState, OkResponse, err};
}

// ── Sub-modules ───────────────────────────────────────────────────────────────

pub mod atlassian;
pub mod bitbucket;
pub mod ctx7;
pub mod docker;
pub mod docs;
pub mod health;
pub mod learning;
pub mod logs;
pub mod mcp;
pub mod schema;
pub mod specs;
pub mod system;
pub mod tests_handler;

pub use atlassian::*;
pub use bitbucket::*;
pub use ctx7::*;
pub use docker::*;
pub use docs::*;
pub use health::*;
pub use learning::*;
pub use logs::*;
pub use mcp::*;
pub use schema::*;
pub use specs::*;
pub use system::*;
pub use tests_handler::*;
