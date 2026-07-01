//! Agents domain — agents tools, embedded prompts, resolution helpers.
//! Embedded agent prompts (.md files in src/agents/) are compiled in at
//! build time and exposed via [`embedded`]. The model an agent runs
//! against is owned by the `model.*` surface
//! (see [[project-model-agent-conversation-ownership]]); per-agent
//! pinning lives on `model::resolve::set_agent_model`.

pub mod agents;
pub mod commands;
pub mod embedded;
pub mod registry;

pub mod resolve;

pub use registry::{
    AgentDef, AgentProvider, CommandDef, HookDef, HookEvent, PromptFragment, SkillDef, SkillFile,
    compose_agents, compose_commands, compose_hooks, compose_prompt_fragments, compose_skills,
    register_provider,
};
