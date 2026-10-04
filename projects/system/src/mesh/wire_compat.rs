//! Refuse to forward camelCase tool args to a peer that predates them.
//!
//! A daemon on a release before [`CAMELCASE_WIRE_SINCE`] reads tool args by
//! their snake_case names and silently ignores unknown keys, so a forwarded
//! `expiresInDays` is dropped and the peer mints a non-expiring token, a
//! `refPath` is dropped from a secret write, and so on — with success reported.
//! Args that carry no camelCase key (single-word fields only, or the types
//! kept snake_case for the mixed window such as `system.update`) are safe and
//! pass, so liveness probes and fleet rolls keep working against old peers.
//!
//! Remove once every daemon is past the cutover.

use anyhow::{Result, bail};

/// The last release whose daemons read tool args as snake_case only.
pub const LAST_SNAKE_WIRE_RELEASE: &str = "0.2.1-rc.11";
/// The first release that reads camelCase tool args.
pub const CAMELCASE_WIRE_SINCE: &str = "0.2.1-rc.12";

/// `true` when any object key in `v` (at any depth) has an uppercase ASCII
/// letter and a non-null value, i.e. a camelCase multi-word field whose value
/// an old peer would drop. A dropped `null` reads as the default anyway.
#[allow(clippy::disallowed_types)]
fn has_camel_key(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(m) => m.iter().any(|(k, child)| {
            (!child.is_null() && k.chars().any(|c| c.is_ascii_uppercase())) || has_camel_key(child)
        }),
        serde_json::Value::Array(a) => a.iter().any(has_camel_key),
        _ => false,
    }
}

/// Pure decision: `Some(reason)` when forwarding `args` for `tool` to a peer
/// reporting `peer_version` would lose fields.
#[allow(clippy::disallowed_types)]
pub fn incompatibility(
    peer: &str,
    tool: &str,
    args: &serde_json::Value,
    peer_version: Option<&str>,
) -> Option<String> {
    let version = peer_version?;
    if crate::update_state::is_newer_full(version, LAST_SNAKE_WIRE_RELEASE) {
        return None;
    }
    if !has_camel_key(args) {
        return None;
    }
    Some(format!(
        "refusing to forward `{tool}` to {peer}: it runs orca {version}, which predates the \
         camelCase wire ({CAMELCASE_WIRE_SINCE}) and would silently ignore some of these \
         arguments. Update that host first (`orca system update --id {peer} --execute`), then \
         retry."
    ))
}

/// Gate a by-peer tool call. The peer's version comes from the liveness cache,
/// or a `mesh/ping` when the cache is cold. An unknown version (the ping
/// failed) is not refused here: the call itself will fail to reach the peer.
#[allow(clippy::disallowed_types)]
pub async fn check(peer_id: &str, peer: &str, tool: &str, args: &serde_json::Value) -> Result<()> {
    if !has_camel_key(args) {
        return Ok(());
    }
    let version = match crate::mesh::peer_info::liveness_if_fresh(peer_id).and_then(|l| l.version) {
        Some(v) => Some(v),
        None => crate::mesh::exec::ping(peer_id).await.version,
    };
    if let Some(reason) = incompatibility(peer, tool, args, version.as_deref()) {
        bail!(reason);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn camel_args_to_a_pre_cutover_peer_are_refused() {
        let args = json!({ "name": "ci", "expiresInDays": 30 });
        let why = incompatibility("thor", "auth.token.create", &args, Some("0.2.1-rc.11"))
            .expect("refused");
        assert!(why.contains("thor") && why.contains("0.2.1-rc.11"), "{why}");
        assert!(why.contains("Update that host first"), "{why}");
        assert!(incompatibility("thor", "t", &args, Some("v0.2.0")).is_some());
    }

    #[test]
    fn nested_camel_keys_count() {
        let args = json!({ "outer": { "refPath": "/x" } });
        assert!(incompatibility("p", "t", &args, Some("0.2.1-rc.11")).is_some());
    }

    #[test]
    fn snake_or_single_word_args_pass_to_old_peers() {
        for args in [
            json!({}),
            json!({ "id": "thor", "name": "x" }),
            json!({ "self_only": true, "release_source": "gitea" }),
            // `system.update`'s flattened retention args serialize as null
            // camelCase keys when unset; dropping a null loses nothing.
            json!({ "self_only": true, "maxMb": null }),
        ] {
            assert!(incompatibility("p", "t", &args, Some("0.2.1-rc.11")).is_none());
        }
    }

    #[test]
    fn current_and_unknown_peers_pass() {
        let args = json!({ "expiresInDays": 30 });
        assert!(incompatibility("p", "t", &args, Some(CAMELCASE_WIRE_SINCE)).is_none());
        assert!(incompatibility("p", "t", &args, Some("0.2.1")).is_none());
        assert!(incompatibility("p", "t", &args, None).is_none());
    }

    #[test]
    fn the_snake_wire_tools_carry_no_camel_keys() {
        // system.update and plugin.serve_asset stay snake_case for the mixed
        // window precisely so they pass this gate against old peers.
        let update = serde_json::to_value(crate::commands::SystemUpdateArgs {
            self_only: true,
            release_source: Some("gitea".into()),
            os_packages: true,
            ..Default::default()
        })
        .unwrap();
        assert!(!has_camel_key(&update), "{update}");
        let serve = serde_json::to_value(crate::plugin_manager::PluginServeAssetArgs {
            name: "a".into(),
            repo_url: "https://github.com/x/a".into(),
            target: "t".into(),
            version: None,
            prerelease: false,
        })
        .unwrap();
        assert!(!has_camel_key(&serve), "{serve}");
    }
}
