//! Agents domain — agents tools, embedded prompts, resolution helpers.
//! Embedded agent prompts (.md files in src/agents/) are compiled in at
//! build time and exposed via [`embedded`]. The model an agent runs
//! against is owned by the `model.*` surface
//! (see [[project-model-agent-conversation-ownership]]); per-agent
//! pinning lives on `llm::resolve::set_agent_model`.

pub mod agents;
pub mod commands;
pub mod embedded;

pub mod resolve;
