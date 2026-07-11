//! The `domain = "agents"` AgentProvider backend for the subprocess entrypoint.
//!
//! orca's plugin-loader reads [`backends_json`] at load time, registers a
//! `domain = "agents"` backend, and thereafter drives it by calling
//! `"{invoke_prefix}.{op}"` across the FFI boundary for each composition op
//! (`agents` / `hooks` / `skills` / `commands` / `prompt_fragments`).
//! [`backend_dispatch`] answers those calls from the compiled-in base roster
//! ([`crate::embedded::BaseRosterProvider`]) and the embedded slash-commands;
//! the toolkit's hybrid `invoke` routes everything else to the `agent.` tool
//! surface.
//!
//! The composition types (`AgentDef` / `CommandDef` / `AgentProvider`) are this
//! crate's own primitives (see [`crate::registry`]); core links the crate as a
//! library for them, and the loader's `domain = "agents"` arm re-registers this
//! backend so the same JSON crosses the FFI boundary unchanged.

use plugin_toolkit::serde_json;

use crate::embedded::BaseRosterProvider;
use crate::registry::{AgentProvider, CommandDef};

/// The op-call prefix the loader drives this backend through. The `__backend`
/// infix guarantees no collision with a real `agent.` tool verb.
const BACKEND_PREFIX: &str = "agent.__backend";

/// Backend descriptor(s) this plugin advertises: one `domain = "agents"`
/// backend whose `invoke_prefix` routes composition ops back to
/// [`backend_dispatch`]. `..Default::default()` keeps the literal forward
/// compatible with new `BackendDef` axes.
pub fn backends_json() -> String {
    let def = plugin_toolkit::abi::BackendDef {
        domain: "agents".to_string(),
        name: "orca-embedded-roster".to_string(),
        invoke_prefix: BACKEND_PREFIX.to_string(),
        ..Default::default()
    };
    serde_json::to_string(&[def]).unwrap_or_else(|_| "[]".to_string())
}

/// Empty schema payload — this plugin advertises no JSON schemas of its own.
pub fn schema_json() -> String {
    "{}".to_string()
}

/// Handle the loader's `agent.__backend.<op>` composition calls. Returns
/// `Some(Ok(json_array))` for a known op, `Some(Err(..))` for an unknown op
/// under our prefix, and `None` for anything else (so the toolkit falls through
/// to the `agent.` tool surface). A JSON encode failure degrades to an empty
/// array rather than taking down `orca install`.
pub fn backend_dispatch(name: &str, _args: &str) -> Option<Result<String, String>> {
    let op = name.strip_prefix(BACKEND_PREFIX)?.strip_prefix('.')?;
    let json = match op {
        "agents" => encode(BaseRosterProvider.agents()),
        "commands" => encode(base_roster_commands()),
        // No hooks/skills/prompt-fragments in the base roster yet.
        "hooks" | "skills" | "prompt_fragments" => "[]".to_string(),
        other => return Some(Err(format!("unknown agents backend op: {other}"))),
    };
    Some(Ok(json))
}

/// The embedded slash-commands as `CommandDef`s, mirroring the `AgentDef` shape
/// [`BaseRosterProvider`] produces for agents. `body` is the verbatim embedded
/// markdown the install flow writes to `~/.claude/commands/<name>.md`.
fn base_roster_commands() -> Vec<CommandDef> {
    crate::commands::embedded_command_names()
        .iter()
        .filter_map(|name| {
            let body = crate::commands::embedded_command(name)?;
            Some(CommandDef {
                name: name.to_string(),
                body: body.to_string(),
                origin: "embedded".to_string(),
            })
        })
        .collect()
}

fn encode<T: plugin_toolkit::serde::Serialize>(items: Vec<T>) -> String {
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}
