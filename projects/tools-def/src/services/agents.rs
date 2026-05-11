//! Service trait for the `agents` domain (list, get prompt, get_config docs,
//! project memory, log search).
//!
//! The service returns structured raw data; the tool layer maps it onto
//! typed Output structs (see `tools-def/src/agents.rs`).

use anyhow::Result;
use async_trait::async_trait;

#[derive(Clone)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
}

#[derive(Clone)]
pub struct MemoryFileData {
    pub name: String,
    pub content: String,
}

#[derive(Clone)]
pub struct ProjectMemoryData {
    pub index: Option<String>,
    pub files: Vec<MemoryFileData>,
}

#[derive(Clone)]
pub struct LogMatchData {
    pub session: String,
    pub role: String,
    pub agent: Option<String>,
    pub content_preview: String,
    pub important: bool,
}

#[derive(Clone)]
pub struct SearchLogsData {
    pub matches: Vec<LogMatchData>,
    /// Optional LLM-generated summary when a local LLM is available.
    pub enhanced_summary: Option<String>,
}

#[async_trait]
pub trait AgentsService: Send + Sync {
    async fn list_agents(&self) -> Result<Vec<AgentInfo>>;

    /// Load the system prompt for `name`. `None` when no such agent.
    async fn get_agent_prompt(&self, name: &str) -> Result<Option<String>>;

    /// All known config-doc basenames (e.g. TOOL_RULES, DELEGATION).
    async fn list_config_docs(&self) -> Result<Vec<String>>;

    /// Read a config doc by basename. `None` when not found.
    async fn read_config_doc(&self, name: &str) -> Result<Option<String>>;

    /// Read all memory files for `project`. `None` when the project memory
    /// directory does not exist.
    async fn read_project_memory(&self, project: &str) -> Result<Option<ProjectMemoryData>>;

    /// Search session logs for `query`. Service may attach an LLM-enhanced
    /// summary when a local model is available.
    async fn search_logs(&self, query: &str, limit: usize) -> Result<SearchLogsData>;
}
