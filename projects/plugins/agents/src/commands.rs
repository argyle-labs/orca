//! Embedded slash-command prompts. Sister to [`crate::embedded`] (agents) —
//! both live in the agents crate so the install flow that materializes
//! `~/.claude/agents/` can also materialize `~/.claude/commands/`.

include!(concat!(env!("OUT_DIR"), "/embedded_commands.rs"));

/// List embedded slash commands as `/name` strings.
pub fn list_embedded_commands() -> Vec<String> {
    embedded_command_names()
        .iter()
        .map(|name| format!("/{name}"))
        .collect()
}
