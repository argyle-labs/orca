//! Agents domain — agents tools, embedded prompts, resolution helpers.
//! Embedded agent prompts (.md files in src/agents/) are compiled in at
//! build time and exposed via [`embedded`]. The model an agent runs
//! against is owned by the `model.*` surface
//! (see [[project-model-agent-conversation-ownership]]); per-agent
//! pinning lives on `model::resolve::set_agent_model`.

pub mod agents;
pub mod commands;
pub mod embedded;
pub mod settings;

pub mod resolve;

// The registry mechanism (trait + registry + FFI bridge) now lives in
// `contract::agents` — the stable core seam an external `argyle-labs/agents`
// plugin implements. Re-exported here so this crate's content modules and
// existing `agents::*` consumers keep their paths during extraction.
pub use contract::agents::{
    AgentDef, AgentProvider, CommandDef, HookDef, HookEvent, PromptFragment, SkillDef, SkillFile,
    compose_agents, compose_commands, compose_hooks, compose_prompt_fragments, compose_skills,
    register_provider,
};
pub use settings::{ClaudeSettings, HookMatcherGroup, hooks_to_settings_tree};
