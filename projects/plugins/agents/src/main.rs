//! Dynamic (subprocess) entrypoint for the agents plugin.
//!
//! The toolkit's `serve_tool_plugin!` emits `fn main`, serving this plugin over
//! the orca socket. Dynamic replacement for the retired cdylib export — the
//! plugin is a `[[bin]]`, owns no runtime, and reaches orca only through the
//! socket (exactly like the retired arr / dockge subprocess plugins).
//!
//! agents is a **hybrid** plugin: it exposes the `agent.` tool surface AND
//! registers a `domain = "agents"` AgentProvider backend. The hybrid arm below
//! serves the `agent.` manifest (filtered from the linked `#[orca_tool]`
//! inventory) alongside the backend hook (the `agent.__backend.*` composition
//! calls the loader makes to drive the base-roster AgentProvider — see
//! [`agents::registration`]).
//!
//! `name: "agent"` (singular) is deliberate — it drives the tool prefix, which
//! must match the `agent.list` / `agent.get` tool names. The crate is `agents`
//! (plural); the backend registers under the loader's `domain = "agents"`.

plugin_toolkit::serve_tool_plugin! {
    name: "agent",
    target_compat: "",
    backends: agents::registration::backends_json(),
    backend_dispatch: agents::registration::backend_dispatch,
}
