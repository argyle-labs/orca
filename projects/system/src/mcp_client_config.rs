//! Reconcile the Claude Code MCP client definition in `~/.claude.json`.
//!
//! Orca owns both facts the client needs — the daemon's HTTP port and an admin
//! token — so the client entry is materialized on every install/update rather
//! than hand-written once. Hand-written entries drift: #538 found the server
//! defined three times with inconsistent env, two of them pointing at the
//! respawn-prone stdio bridge.
//!
//! The functions here are pure (they take and return `serde_json::Value`) so
//! the reconcile rules are testable without touching a real config file.

// `~/.claude.json` is Claude Code's file, not ours: it carries arbitrary
// client keys (projects, history, per-project tool grants) that we must round
// trip untouched while editing one nested map. Modelling it as a struct would
// silently drop every field we didn't declare.
#![allow(clippy::disallowed_types)]

use serde_json::{Value, json};

/// What a reconcile pass changed. Empty on a no-op run so the install report
/// can say "already correct" instead of claiming a write it didn't make.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// The canonical entry was absent or differed and was (re)written.
    pub entry_written: bool,
    /// Legacy-named entries removed from the global `mcpServers` map.
    pub pruned_global: usize,
    /// Entries (either name) removed from per-project `mcpServers` maps.
    pub pruned_project: usize,
}

impl Reconciled {
    pub fn changed(&self) -> bool {
        self.entry_written || self.pruned_global > 0 || self.pruned_project > 0
    }
}

/// Build the canonical client entry: HTTP transport pointed at the daemon's
/// JSON-RPC endpoint. `token` is omitted rather than written blank — a blank
/// bearer is rejected by the daemon and is harder to diagnose than a missing
/// header.
pub fn desired_entry(daemon_url: &str, token: Option<&str>) -> Value {
    let url = format!("{}/api/mcp", daemon_url.trim_end_matches('/'));
    let mut entry = json!({ "type": "http", "url": url });
    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
        entry["headers"] = json!({ "Authorization": format!("Bearer {t}") });
    }
    entry
}

/// Pull the bearer token out of an existing entry, if it carries one. Lets a
/// reconcile preserve a working credential when this pass has no token to mint
/// — losing a valid token would 401 every tool call.
pub fn existing_token(entry: &Value) -> Option<String> {
    let auth = entry.get("headers")?.get("Authorization")?.as_str()?;
    let token = auth.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Reconcile `root` (a parsed `~/.claude.json`) to hold exactly one definition
/// of the orca MCP server: the canonical entry under `name`, globally.
///
/// - Rewrites the global entry when absent or different.
/// - Removes `legacy`-named entries from the global map.
/// - Removes BOTH names from every per-project map — a project-scoped
///   definition silently shadows the global one, which is how the inconsistent
///   3x state in #538 stayed invisible.
pub fn reconcile(root: &mut Value, name: &str, legacy: &str, desired: Value) -> Reconciled {
    let mut out = Reconciled::default();

    if !root.is_object() {
        *root = json!({});
    }

    // Global map: write the canonical entry, drop the legacy name.
    let servers = root
        .as_object_mut()
        .expect("root forced to object above")
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    if let Some(map) = servers.as_object_mut() {
        if map.remove(legacy).is_some() {
            out.pruned_global += 1;
        }
        if map.get(name) != Some(&desired) {
            map.insert(name.to_string(), desired);
            out.entry_written = true;
        }
    }

    // Per-project maps: both names are redundant once the global entry is
    // correct, and a stale one here wins over it.
    if let Some(projects) = root.get_mut("projects").and_then(Value::as_object_mut) {
        for (_path, project) in projects.iter_mut() {
            let Some(map) = project.get_mut("mcpServers").and_then(Value::as_object_mut) else {
                continue;
            };
            for key in [name, legacy] {
                if map.remove(key).is_some() {
                    out.pruned_project += 1;
                }
            }
        }
    }

    out
}

/// Remove every definition of the given names — global and per-project. Used by
/// uninstall so it tears out exactly what install put in, including any legacy
/// name still lying around: a name we no longer write is a name we clean up, not
/// one we leave behind for the next install to trip over.
pub fn purge(root: &mut Value, names: &[&str]) -> Reconciled {
    let mut out = Reconciled::default();

    if let Some(map) = root.get_mut("mcpServers").and_then(Value::as_object_mut) {
        for name in names {
            if map.remove(*name).is_some() {
                out.pruned_global += 1;
            }
        }
    }

    if let Some(projects) = root.get_mut("projects").and_then(Value::as_object_mut) {
        for (_path, project) in projects.iter_mut() {
            let Some(map) = project.get_mut("mcpServers").and_then(Value::as_object_mut) else {
                continue;
            };
            for name in names {
                if map.remove(*name).is_some() {
                    out.pruned_project += 1;
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_entry_is_http_with_bearer_header() {
        let e = desired_entry("http://127.0.0.1:12000", Some("tok123"));
        assert_eq!(e["type"], "http");
        assert_eq!(e["url"], "http://127.0.0.1:12000/api/mcp");
        assert_eq!(e["headers"]["Authorization"], "Bearer tok123");
    }

    #[test]
    fn desired_entry_omits_header_without_token() {
        // Blank and absent both mean "no credential" — never write `Bearer `.
        for tok in [None, Some(""), Some("   ")] {
            let e = desired_entry("http://127.0.0.1:12000", tok);
            assert!(e.get("headers").is_none(), "unexpected header for {tok:?}");
        }
    }

    #[test]
    fn desired_entry_does_not_double_slash_a_trailing_slash_url() {
        let e = desired_entry("http://127.0.0.1:12000/", None);
        assert_eq!(e["url"], "http://127.0.0.1:12000/api/mcp");
    }

    #[test]
    fn existing_token_round_trips_and_rejects_junk() {
        let e = desired_entry("http://x", Some("abc"));
        assert_eq!(existing_token(&e).as_deref(), Some("abc"));
        assert_eq!(existing_token(&json!({})), None);
        assert_eq!(existing_token(&json!({"headers": {}})), None);
        // Present but empty must not resurrect as a blank credential.
        assert_eq!(
            existing_token(&json!({"headers": {"Authorization": "Bearer "}})),
            None
        );
    }

    #[test]
    fn reconcile_writes_entry_into_empty_config() {
        let mut root = json!({});
        let d = desired_entry("http://127.0.0.1:12000", Some("t"));
        let out = reconcile(&mut root, "orca", "orca-local", d.clone());

        assert!(out.entry_written);
        assert_eq!(root["mcpServers"]["orca"], d);
    }

    #[test]
    fn reconcile_is_idempotent() {
        let mut root = json!({});
        let d = desired_entry("http://127.0.0.1:12000", Some("t"));
        reconcile(&mut root, "orca", "orca-local", d.clone());
        let second = reconcile(&mut root, "orca", "orca-local", d);

        assert!(
            !second.changed(),
            "second pass should be a no-op: {second:?}"
        );
    }

    #[test]
    fn reconcile_collapses_the_538_three_definition_state() {
        // Exactly the shape #538 found: a global stdio entry plus two
        // project-scoped ones, all under the legacy name.
        let mut root = json!({
            "mcpServers": {
                "orca-local": { "type": "stdio", "command": "orca", "args": ["mcp-serve"] }
            },
            "projects": {
                "/Users/x/code/orca":  { "mcpServers": { "orca-local": { "type": "stdio" } } },
                "/Users/x/code/rebuy": { "mcpServers": { "orca-local": { "type": "stdio" } } }
            }
        });

        let d = desired_entry("http://127.0.0.1:12000", Some("t"));
        let out = reconcile(&mut root, "orca", "orca-local", d.clone());

        assert_eq!(out.pruned_global, 1);
        assert_eq!(out.pruned_project, 2);
        assert!(out.entry_written);

        assert_eq!(root["mcpServers"]["orca"], d);
        assert!(root["mcpServers"].get("orca-local").is_none());
        for p in ["/Users/x/code/orca", "/Users/x/code/rebuy"] {
            let map = &root["projects"][p]["mcpServers"];
            assert!(map.get("orca-local").is_none(), "{p} kept legacy entry");
            assert!(map.get("orca").is_none(), "{p} kept a shadowing entry");
        }
    }

    #[test]
    fn reconcile_rewrites_a_stale_port_or_token() {
        let mut root = json!({
            "mcpServers": { "orca": desired_entry("http://127.0.0.1:9999", Some("old")) }
        });
        let d = desired_entry("http://127.0.0.1:12000", Some("new"));
        let out = reconcile(&mut root, "orca", "orca-local", d.clone());

        assert!(out.entry_written, "drifted entry must be rewritten");
        assert_eq!(root["mcpServers"]["orca"], d);
    }

    #[test]
    fn reconcile_preserves_unrelated_servers_and_project_keys() {
        let mut root = json!({
            "mcpServers": { "other": { "type": "stdio", "command": "x" } },
            "projects": {
                "/p": { "mcpServers": { "other": { "type": "stdio" } }, "allowedTools": ["a"] }
            },
            "someTopLevelKey": 42
        });

        reconcile(
            &mut root,
            "orca",
            "orca-local",
            desired_entry("http://u", None),
        );

        assert_eq!(root["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            root["projects"]["/p"]["mcpServers"]["other"]["type"],
            "stdio"
        );
        assert_eq!(root["projects"]["/p"]["allowedTools"][0], "a");
        assert_eq!(root["someTopLevelKey"], 42);
    }

    #[test]
    fn reconcile_repairs_a_non_object_config_instead_of_panicking() {
        // A corrupt or unexpected shape must not abort the install step.
        let mut root = json!("not an object");
        let d = desired_entry("http://u", None);
        let out = reconcile(&mut root, "orca", "orca-local", d.clone());

        assert!(out.entry_written);
        assert_eq!(root["mcpServers"]["orca"], d);
    }

    #[test]
    fn purge_removes_both_names_everywhere() {
        let mut root = json!({
            "mcpServers": {
                "orca":       { "type": "http" },
                "orca-local": { "type": "stdio" },
                "other":      { "type": "stdio" }
            },
            "projects": {
                "/a": { "mcpServers": { "orca-local": {}, "other": {} } },
                "/b": { "mcpServers": { "orca": {} } }
            }
        });

        let out = purge(&mut root, &["orca", "orca-local"]);

        assert_eq!(out.pruned_global, 2);
        assert_eq!(out.pruned_project, 2);
        assert!(root["mcpServers"].get("orca").is_none());
        assert!(root["mcpServers"].get("orca-local").is_none());
        // Everything that isn't ours survives.
        assert_eq!(root["mcpServers"]["other"]["type"], "stdio");
        assert!(root["projects"]["/a"]["mcpServers"].get("other").is_some());
    }

    #[test]
    fn purge_on_a_clean_config_is_a_no_op() {
        let mut root = json!({ "mcpServers": { "other": {} } });
        let out = purge(&mut root, &["orca", "orca-local"]);

        assert!(!out.changed(), "nothing of ours present: {out:?}");
    }

    #[test]
    fn purge_tolerates_a_config_with_no_mcpservers_at_all() {
        let mut root = json!({ "projects": {} });
        assert!(!purge(&mut root, &["orca"]).changed());
    }

    #[test]
    fn reconcile_replaces_a_non_object_mcpservers_value() {
        let mut root = json!({ "mcpServers": "garbage" });
        let d = desired_entry("http://u", None);
        reconcile(&mut root, "orca", "orca-local", d.clone());

        assert_eq!(root["mcpServers"]["orca"], d);
    }
}
