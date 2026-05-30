//! Agents domain — agents + agent_backend tools and their service traits.
//! Embedded agent prompts (.md files in src/agents/) are compiled in at
//! build time and exposed via [`embedded`].

pub mod agent_backend;
pub mod agents;
pub mod commands;
pub mod commands_tool;
pub mod embedded;

pub mod resolve;
