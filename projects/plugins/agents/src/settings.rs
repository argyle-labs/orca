//! Typed model of Claude Code's `~/.claude/settings.json`.
//!
//! The hooks composition sink writes composed [`HookDef`]s into `settings.json`
//! without opaque JSON: every field is a typed struct/enum (no
//! `serde_json::Value`, no catch-all map). Per the capability-registry design
//! (see `docs/CAPABILITY-REGISTRIES.md`), orca **owns** the file's `hooks`
//! subtree — when it writes, it round-trips through [`ClaudeSettings`], so any
//! settings key orca does not model is intentionally dropped. Callers therefore
//! only write when there is at least one hook to materialize, leaving a
//! hand-managed settings file untouched in the common (no-hook) case.
//!
//! `settings.json` shape for hooks:
//! ```json
//! { "hooks": { "PreToolUse": [ { "matcher": "Write|Edit",
//!     "hooks": [ { "type": "command", "command": "…" } ] } ] } }
//! ```

use crate::registry::{HookDef, HookEvent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The subset of Claude Code `settings.json` orca models. Unmodeled keys are
/// dropped on write — orca only rewrites this file when it has hooks to
/// materialize, so a hand-edited settings file is left alone otherwise.
/// Every field is `Option`/collection with `skip_serializing_if` so a round
/// trip emits only what is actually set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ClaudeSettings {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(
        rename = "alwaysThinkingEnabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub always_thinking_enabled: Option<bool>,
    #[serde(rename = "cleanupPeriodDays", skip_serializing_if = "Option::is_none")]
    pub cleanup_period_days: Option<u64>,
    #[serde(rename = "respectGitignore", skip_serializing_if = "Option::is_none")]
    pub respect_gitignore: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub env: BTreeMap<String, String>,
    #[serde(
        rename = "enabledPlugins",
        skip_serializing_if = "BTreeMap::is_empty",
        default
    )]
    pub enabled_plugins: BTreeMap<String, bool>,
    #[serde(
        rename = "enableAllProjectMcpServers",
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_all_project_mcp_servers: Option<bool>,
    #[serde(
        rename = "enabledMcpjsonServers",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub enabled_mcpjson_servers: Vec<String>,
    #[serde(
        rename = "disabledMcpjsonServers",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub disabled_mcpjson_servers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
    /// Hook bindings keyed by lifecycle event. This is the subtree orca owns.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub hooks: BTreeMap<HookEvent, Vec<HookMatcherGroup>>,
}

/// `permissions` block. Rules are plain strings (e.g. `"Bash(git *)"`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Permissions {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub allow: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub deny: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ask: Vec<String>,
    #[serde(rename = "defaultMode", skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,
    #[serde(
        rename = "additionalDirectories",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub additional_directories: Vec<String>,
}

/// `attribution` block — controls commit/PR trailers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Attribution {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
}

/// One matcher group under a hook event: a tool matcher plus the commands that
/// run for it. An empty `matcher` means "all tools".
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HookMatcherGroup {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub matcher: String,
    pub hooks: Vec<HookCommand>,
}

/// A single command hook. Only the `command` type is emitted by composition —
/// `prompt`/`agent` hook types are authored by the user, not orca.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HookCommand {
    #[serde(rename = "type")]
    pub kind: HookCommandKind,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// Hook execution type. Composition only emits `command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HookCommandKind {
    Command,
}

/// Fold composed hooks into the `settings.json` hooks tree: group by event,
/// then by matcher (preserving contribution order within a matcher). Returns an
/// empty map when there are no hooks, so callers can decide not to touch the
/// file at all.
pub fn hooks_to_settings_tree(hooks: &[HookDef]) -> BTreeMap<HookEvent, Vec<HookMatcherGroup>> {
    let mut by_event: BTreeMap<HookEvent, Vec<HookMatcherGroup>> = BTreeMap::new();
    for hook in hooks {
        let groups = by_event.entry(hook.event).or_default();
        let command = HookCommand {
            kind: HookCommandKind::Command,
            command: hook.command.clone(),
            timeout: None,
        };
        // Merge into an existing group with the same matcher, else start one.
        match groups.iter_mut().find(|g| g.matcher == hook.matcher) {
            Some(group) => group.hooks.push(command),
            None => groups.push(HookMatcherGroup {
                matcher: hook.matcher.clone(),
                hooks: vec![command],
            }),
        }
    }
    by_event
}

// HookEvent is used as a JSON object key. serde_json requires string keys;
// HookEvent's unit variants serialize to their PascalCase names, which works
// directly as a map key — no manual key handling needed.

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: HookEvent, matcher: &str, command: &str) -> HookDef {
        HookDef {
            event,
            matcher: matcher.to_string(),
            command: command.to_string(),
            origin: "test".to_string(),
        }
    }

    #[test]
    fn empty_hooks_produce_empty_tree() {
        assert!(hooks_to_settings_tree(&[]).is_empty());
    }

    #[test]
    fn groups_by_event_then_matcher() {
        let tree = hooks_to_settings_tree(&[
            hook(HookEvent::PostToolUse, "Write|Edit", "fmt"),
            hook(HookEvent::PostToolUse, "Write|Edit", "lint"),
            hook(HookEvent::PostToolUse, "Bash", "log"),
            hook(HookEvent::Stop, "", "notify"),
        ]);
        let post = &tree[&HookEvent::PostToolUse];
        assert_eq!(post.len(), 2, "two distinct matchers under PostToolUse");
        let we = post.iter().find(|g| g.matcher == "Write|Edit").unwrap();
        assert_eq!(we.hooks.len(), 2, "both commands merged under one matcher");
        let stop = &tree[&HookEvent::Stop];
        assert_eq!(stop[0].matcher, "");
    }

    #[test]
    fn serializes_to_expected_settings_shape() {
        let settings = ClaudeSettings {
            hooks: hooks_to_settings_tree(&[hook(HookEvent::PreToolUse, "Bash", "guard.sh")]),
            ..Default::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        // Only the hooks subtree is present; no null/empty keys leak.
        assert_eq!(
            json,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"guard.sh"}]}]}}"#
        );
    }

    #[test]
    fn round_trips_preserving_modeled_keys() {
        let src = r#"{"model":"opus","permissions":{"allow":["Bash(git *)"]},"env":{"DEBUG":"1"}}"#;
        let parsed: ClaudeSettings = serde_json::from_str(src).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("opus"));
        assert_eq!(
            parsed.permissions.as_ref().unwrap().allow,
            vec!["Bash(git *)"]
        );
        assert_eq!(parsed.env.get("DEBUG").map(String::as_str), Some("1"));
    }
}
