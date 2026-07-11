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
pub mod settings;

pub mod resolve;

pub use registry::{
    AgentDef, AgentProvider, CommandDef, HookDef, HookEvent, InvokeThunk, PromptFragment, SkillDef,
    SkillFile, compose_agents, compose_commands, compose_hooks, compose_prompt_fragments,
    compose_skills, deregister_provider, register_from_def, register_from_json, register_provider,
};
pub use settings::{ClaudeSettings, HookMatcherGroup, hooks_to_settings_tree};
