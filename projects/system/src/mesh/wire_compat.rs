//! Refuse to forward renamed tool args to a peer that predates the camelCase
//! wire.
//!
//! A daemon before the camelCase wire reads tool args by their snake_case
//! names and silently ignores unknown keys, so a forwarded `expiresInDays` is
//! dropped and the peer mints a non-expiring token, a `refPath` is dropped from
//! a secret write, and so on — with success reported.
//!
//! Only args whose name changed are gated. [`RENAMED_ARGS`] lists them per
//! tool, and inventory-tests checks it against the rc.11 args schema, so args
//! that were already camelCase there, free-form maps (env vars, headers,
//! labels) and single-word fields pass untouched.
//!
//! A peer reads the camelCase wire iff its `mesh/ping` result carries
//! `camel_wire: true`; older daemons omit the field. A failed ping refuses the
//! call, since nothing proves the peer would keep the arguments.
//!
//! Remove once every daemon reports `camel_wire`.

use anyhow::{Context, Result, bail};

/// Args renamed snake_case → camelCase since rc.11, per tool. Must match the
/// rc.11 schema exactly (`wire_compat_renamed_args_match_the_rc11_schema`).
pub const RENAMED_ARGS: &[(&str, &[&str])] = &[
    ("auth.token.create", &["canMutate", "expiresInDays"]),
    ("model.list", &["enabledOnly"]),
    ("pki.create", &["pluginId"]),
    ("plugin.data.detail", &["dataKey"]),
    ("secrets.upsert", &["refPath", "valueStdin"]),
    ("storage.share.create", &["optionsRendered"]),
    (
        "system.build",
        &[
            "codesignIdentity",
            "outDir",
            "pkgSignIdentity",
            "plgBinaryUrl",
            "plgUrl",
        ],
    ),
    ("system.install", &["adminPubkey", "homeDir", "serviceUser"]),
];

/// Renamed keys of `tool` present with a non-null value at any depth of
/// `args`. A dropped `null` reads as the default anyway.
#[allow(clippy::disallowed_types)]
fn renamed_keys_present(tool: &str, args: &serde_json::Value) -> Vec<&'static str> {
    let Some((_, renamed)) = RENAMED_ARGS.iter().find(|(t, _)| *t == tool) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect(args, renamed, &mut found);
    found
}

#[allow(clippy::disallowed_types)]
fn collect(v: &serde_json::Value, renamed: &[&'static str], found: &mut Vec<&'static str>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, child) in m {
                if !child.is_null()
                    && let Some(r) = renamed.iter().find(|r| **r == k)
                    && !found.contains(r)
                {
                    found.push(r);
                }
                collect(child, renamed, found);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|c| collect(c, renamed, found)),
        _ => {}
    }
}

/// Pure decision: `Some(reason)` when forwarding `args` for `tool` to `peer`
/// could lose fields. `camel_wire` is the peer's ping marker, or the ping
/// error when the peer could not be asked.
#[allow(clippy::disallowed_types)]
pub fn incompatibility(
    peer: &str,
    tool: &str,
    args: &serde_json::Value,
    camel_wire: Result<bool, String>,
) -> Option<String> {
    let keys = renamed_keys_present(tool, args);
    if keys.is_empty() {
        return None;
    }
    let keys = keys.join(", ");
    match camel_wire {
        Ok(true) => None,
        Ok(false) => Some(format!(
            "refusing to forward `{tool}` to {peer}: it predates the camelCase wire and would \
             silently ignore {keys}. Update that host first \
             (`orca system update --id {peer} --execute`), then retry."
        )),
        Err(e) => Some(format!(
            "refusing to forward `{tool}` to {peer}: could not confirm it reads the camelCase \
             wire ({e}), and an older peer would silently ignore {keys}."
        )),
    }
}

/// Gate a by-peer tool call. Pings the peer only when `args` carries a renamed
/// key, so ordinary calls pay nothing.
#[allow(clippy::disallowed_types)]
pub async fn check(
    peer_id: &str,
    peer: &str,
    targets: &[String],
    tool: &str,
    args: &serde_json::Value,
) -> Result<()> {
    if renamed_keys_present(tool, args).is_empty() {
        return Ok(());
    }
    let camel_wire = peer_reads_camel_wire(peer_id, targets)
        .await
        .map_err(|e| format!("{e:#}"));
    if let Some(reason) = incompatibility(peer, tool, args, camel_wire) {
        bail!(reason);
    }
    Ok(())
}

async fn peer_reads_camel_wire(peer_id: &str, targets: &[String]) -> Result<bool> {
    let pong = crate::mesh::dialer::try_targets_tracked(Some(peer_id), targets, |t| async move {
        crate::mesh::ping(&t).await
    })
    .await
    .with_context(|| format!("mesh/ping {peer_id}"))?;
    Ok(pong.camel_wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renamed_args_to_a_pre_cutover_peer_are_refused() {
        let args = json!({ "name": "ci", "expiresInDays": 30 });
        let why = incompatibility("thor", "auth.token.create", &args, Ok(false)).expect("refused");
        assert!(
            why.contains("thor") && why.contains("expiresInDays"),
            "{why}"
        );
        assert!(why.contains("Update that host first"), "{why}");
    }

    #[test]
    fn nested_renamed_keys_count() {
        let args = json!({ "outer": { "refPath": "/x" } });
        assert!(incompatibility("p", "secrets.upsert", &args, Ok(false)).is_some());
    }

    #[test]
    fn a_failed_ping_fails_closed() {
        let args = json!({ "expiresInDays": 30 });
        let why = incompatibility("p", "auth.token.create", &args, Err("timed out".into()))
            .expect("refused");
        assert!(why.contains("timed out"), "{why}");
    }

    #[test]
    fn camel_wire_peers_pass() {
        let args = json!({ "expiresInDays": 30 });
        assert!(incompatibility("p", "auth.token.create", &args, Ok(true)).is_none());
    }

    #[test]
    fn args_camelcase_at_rc11_pass_to_old_peers() {
        // Already camelCase at rc.11, or free-form maps whose keys are data:
        // none is a renamed field, so an old peer reads them all.
        for (tool, args) in [
            (
                "container.create",
                json!({ "restartPolicy": "always", "env": { "TZ": "UTC", "PATH": "/bin" } }),
            ),
            (
                "model.create",
                json!({ "baseUrl": "http://x", "apiKeyRef": "k" }),
            ),
            (
                "plugin.create",
                json!({ "repoUrl": "https://x", "releaseSource": "gitea" }),
            ),
            (
                "notify.create",
                json!({ "headers": { "X-Token": "t" }, "labels": { "Team": "a" } }),
            ),
            ("config.set", json!({ "value": { "MaxSize": 3 } })),
            (
                "auth.token.create",
                json!({ "name": "ci", "role": "admin" }),
            ),
        ] {
            assert!(
                incompatibility("p", tool, &args, Ok(false)).is_none(),
                "{tool} {args}"
            );
        }
    }

    #[test]
    fn null_renamed_keys_pass() {
        let args = json!({ "name": "ci", "expiresInDays": null });
        assert!(incompatibility("p", "auth.token.create", &args, Ok(false)).is_none());
    }

    #[test]
    fn the_snake_wire_tools_carry_no_renamed_keys() {
        for tool in ["system.update", "plugin.serve_asset"] {
            assert!(
                !RENAMED_ARGS.iter().any(|(t, _)| *t == tool),
                "{tool} stays snake_case on the wire and must never be gated"
            );
        }
    }
}
