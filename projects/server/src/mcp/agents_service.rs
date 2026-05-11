//! `AgentsService` impl — thin shim over agents listing, agent prompts,
//! config-doc registry, project memory, and session-log search.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::agents::{
    AgentInfo, AgentsService, LogMatchData, MemoryFileData, ProjectMemoryData, SearchLogsData,
};
use orca_utils::config::Config;
use std::sync::Arc;

use crate::serve::api::llm as local_llm;

pub struct ServerAgents {
    pub config: Arc<Config>,
}

#[async_trait]
impl AgentsService for ServerAgents {
    async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(crate::agents::list_embedded_agents()
            .into_iter()
            .map(|(name, description)| AgentInfo { name, description })
            .collect())
    }

    async fn get_agent_prompt(&self, name: &str) -> Result<Option<String>> {
        Ok(crate::mcp::agent_resolve::load_agent_prompt(name, &self.config))
    }

    async fn list_config_docs(&self) -> Result<Vec<String>> {
        Ok(orca_utils::config::docs::list_basenames())
    }

    async fn read_config_doc(&self, name: &str) -> Result<Option<String>> {
        Ok(orca_utils::config::docs::get(name))
    }

    async fn read_project_memory(&self, project: &str) -> Result<Option<ProjectMemoryData>> {
        let dir = self.config.memory_root.join(project);
        if !dir.exists() {
            return Ok(None);
        }
        let index_path = dir.join("MEMORY.md");
        let index = if index_path.exists() {
            Some(std::fs::read_to_string(&index_path)?)
        } else {
            None
        };
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?
            .flatten()
            .filter(|e| {
                let p = e.path();
                p.extension().map(|x| x == "md").unwrap_or(false)
                    && p.file_name().map(|n| n != "MEMORY.md").unwrap_or(true)
            })
            .collect();
        entries.sort_by_key(|e| e.file_name());
        let mut files = Vec::new();
        for f in entries {
            let path = f.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let content = std::fs::read_to_string(&path)?;
            files.push(MemoryFileData { name, content });
        }
        Ok(Some(ProjectMemoryData { index, files }))
    }

    async fn search_logs(&self, query: &str, limit: usize) -> Result<SearchLogsData> {
        let raw = crate::conversation::log::search_logs(&self.config.logs_dir(), query, limit)?;
        let matches: Vec<LogMatchData> = raw
            .iter()
            .map(|m| {
                let agent = m["agent"].as_str().unwrap_or("");
                LogMatchData {
                    session: m["session"].as_str().unwrap_or("?").to_string(),
                    role: m["role"].as_str().unwrap_or("?").to_string(),
                    agent: (!agent.is_empty()).then(|| agent.to_string()),
                    content_preview: m["content"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect(),
                    important: m["important"].as_bool() == Some(true),
                }
            })
            .collect();
        // Best-effort LLM summarization when a local model is available.
        let enhanced_summary = if !matches.is_empty()
            && let Some(llm) = local_llm::discover_local_llm().await
        {
            let raw_text = format!(
                "Found {} match(es):\n{}",
                matches.len(),
                matches
                    .iter()
                    .map(|m| format!(
                        "{} [{}{}] {}{}",
                        m.session,
                        m.role,
                        m.agent
                            .as_deref()
                            .map(|a| format!("/@{a}"))
                            .unwrap_or_default(),
                        m.content_preview,
                        if m.important { " ★" } else { "" }
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            local_llm::present_text_results(&llm, query, &raw_text, 8000).await
        } else {
            None
        };
        Ok(SearchLogsData {
            matches,
            enhanced_summary,
        })
    }
}
