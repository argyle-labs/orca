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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_embedded_commands_prefixes_slash() {
        // Every entry must start with "/" and contain no whitespace —
        // the build script materializes these directly into a CLI surface.
        for cmd in list_embedded_commands() {
            assert!(cmd.starts_with('/'), "missing slash: {cmd}");
            assert!(!cmd.contains(' '), "whitespace in command: {cmd}");
            assert!(cmd.len() > 1, "empty command name: {cmd}");
        }
    }

    #[test]
    fn embedded_command_names_match_listed() {
        let listed = list_embedded_commands();
        let raw: Vec<&str> = embedded_command_names().to_vec();
        assert_eq!(listed.len(), raw.len());
        for (a, b) in listed.iter().zip(raw.iter()) {
            assert_eq!(a, &format!("/{b}"));
        }
    }
}
