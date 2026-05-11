//! Server-coupled agent_backend tools — `OrcaTool::run` impls only.
//!
//! The metadata + Args/Output definitions live in
//! `orca-tools-def::agent_backend`. These four files contribute only the
//! native `run` bodies that reach into server internals (config +
//! AgentBackend). Registration with the ToolRegistry is driven by
//! `orca_tools_def::native_register()` via the `declare_tools!` macro.

mod backend_override;
mod set_mode;
mod status;
mod use_server_anthropic;
