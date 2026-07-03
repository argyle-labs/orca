//! Secrets — host-level secret metadata with pluggable backends.
//!
//! The `secrets` table records `{name, backend, ref_path, description}`. For
//! the `inline` backend, the actual value lives in `settings` under the
//! `secrets.{name}` prefix (so existing `settings::secret_*` helpers and the
//! `auth.login` path stay interoperable). For external backends (op, bw,
//! keychain, …) the value is fetched on demand and `ref_path` is the vendor-
//! specific address.

use anyhow::Result;
use rusqlite::Connection;

use crate::settings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRecord {
    pub name: String,
    pub backend: String,
    pub ref_path: String,
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub fn list(conn: &Connection) -> Result<Vec<SecretRecord>> {
    let mut stmt = conn.prepare(
        "SELECT name, backend, ref_path, description, created_at, updated_at
         FROM secrets ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SecretRecord {
            name: row.get(0)?,
            backend: row.get(1)?,
            ref_path: row.get(2)?,
            description: row.get(3)?,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<SecretRecord>> {
    let result = conn.query_row(
        "SELECT name, backend, ref_path, description, created_at, updated_at
         FROM secrets WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(SecretRecord {
                name: row.get(0)?,
                backend: row.get(1)?,
                ref_path: row.get(2)?,
                description: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        },
    );
    match result {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Pod-mesh storage gate. Refuses secret writes when this host has joined a
/// pod but is not flagged secure (pod_self.self_secure = 0). Hosts that
/// have never run `orca pod init` or `pod join` (no pod_self row) bypass
/// the gate so non-pod workflows still work.
fn ensure_self_secure(conn: &Connection) -> Result<()> {
    use rusqlite::OptionalExtension;
    let v: Option<i64> = conn
        .query_row("SELECT self_secure FROM pod_self WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    match v {
        None => Ok(()), // host hasn't joined a pod — no gating
        Some(1) => Ok(()),
        Some(_) => anyhow::bail!(
            "secrets storage disabled on this host — run `orca pod self-secure on` to enable"
        ),
    }
}

/// Upsert a metadata row. Returns true if the row was created, false on update.
pub fn upsert(
    conn: &Connection,
    name: &str,
    backend: &str,
    ref_path: &str,
    description: Option<&str>,
) -> Result<bool> {
    ensure_self_secure(conn)?;
    let existed = conn
        .query_row(
            "SELECT 1 FROM secrets WHERE name = ?1",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .is_ok();
    conn.execute(
        "INSERT INTO secrets (name, backend, ref_path, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4,
                 strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(name) DO UPDATE SET
             backend     = excluded.backend,
             ref_path    = excluded.ref_path,
             description = excluded.description,
             updated_at  = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![name, backend, ref_path, description],
    )?;
    Ok(!existed)
}

/// Delete the metadata row and (for inline) any stored value. Returns true if anything was removed.
pub fn delete(conn: &Connection, name: &str) -> Result<bool> {
    // Look up the record first so we know whether to also clean the inline value.
    let record = get(conn, name)?;
    let mut removed = false;
    if let Some(r) = &record {
        let n = conn.execute(
            "DELETE FROM secrets WHERE name = ?1",
            rusqlite::params![name],
        )?;
        removed = n > 0;
        if r.backend == "inline" {
            // Best-effort: ignore "not present" since the metadata is the
            // source of truth for whether the secret existed.
            let _ = settings::secret_delete(conn, name)?;
        }
    }
    Ok(removed)
}

/// Read the inline-stored value for `name`. Returns `None` if no value is stored
/// (caller should check that the record's `backend` is `inline` before invoking).
pub fn read_inline_value(conn: &Connection, name: &str) -> Result<Option<String>> {
    settings::secret_get(conn, name)
}

/// Store the inline value for `name`. The metadata row should already exist
/// (caller `upsert`s first).
pub fn write_inline_value(conn: &Connection, name: &str, value: &str) -> Result<()> {
    ensure_self_secure(conn)?;
    settings::secret_set(conn, name, value)
}

/// Multi-instance secret convention. Keys follow `<provider>.<instance>.<field>`
/// (e.g. `proxmox.delta.api_url`, `proxmox.delta.api_token`). Returns one
/// entry per `instance_id` with all its `field -> value` pairs resolved
/// (inline backend only — external backends are skipped with a warn).
///
/// Used by colocated API collectors (proxmox, unraid, plex, sonarr, ...) to
/// enumerate every instance configured locally. N instances per provider —
/// the helper assumes nothing about cardinality.
pub fn list_provider_instances(
    conn: &Connection,
    provider: &str,
) -> Result<Vec<(String, std::collections::BTreeMap<String, String>)>> {
    use std::collections::BTreeMap;
    let prefix = format!("{provider}.");
    let mut stmt =
        conn.prepare("SELECT name, backend FROM secrets WHERE name LIKE ?1 || '%' ORDER BY name")?;
    let rows = stmt.query_map(rusqlite::params![&prefix], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for row in rows {
        let (name, backend) = row?;
        let rest = match name.strip_prefix(&prefix) {
            Some(r) => r,
            None => continue,
        };
        let (instance, field) = match rest.split_once('.') {
            Some((i, f)) if !i.is_empty() && !f.is_empty() => (i, f),
            _ => continue,
        };
        if backend != "inline" {
            tracing::warn!(
                secret = %name, backend = %backend,
                "list_provider_instances skipping non-inline backend"
            );
            continue;
        }
        let Some(value) = settings::secret_get(conn, &name)? else {
            continue;
        };
        out.entry(instance.to_string())
            .or_default()
            .insert(field.to_string(), value);
    }
    Ok(out.into_iter().collect())
}

// ── Host secrets service: run a plugin's SecretOp on core's pooled connection ──
//
// Bound into every plugin via `PluginMod::set_secret_op`. `plugin_toolkit::secrets`
// otherwise opens its OWN connection to run this same SQL, racing the daemon's
// on the WAL/shm index (SHMOPEN 5898). Core owns the crypto + the tables, so the
// whole op runs here.

/// The inline backend tag — a secret whose value lives in orca's own encrypted
/// store (mirrors `plugin_toolkit::secrets::BACKEND_INLINE`).
const BACKEND_INLINE: &str = "inline";

/// Execute one plugin secrets op on `conn`. Replicates the resolution
/// `plugin_toolkit::secrets` performed locally (inline decrypt; external
/// backends are not resolvable on this host yet).
pub fn exec_secret_op(
    conn: &Connection,
    op: &plugin_abi::SecretOp,
) -> Result<plugin_abi::SecretReply> {
    use plugin_abi::{SecretOp, SecretReply};
    match op {
        SecretOp::Get { name } => {
            let Some(row) = get(conn, name)? else {
                return Ok(SecretReply::default());
            };
            let value = match row.backend.as_str() {
                BACKEND_INLINE => Some(read_inline_value(conn, &row.name)?.ok_or_else(|| {
                    anyhow::anyhow!("inline secret '{}' has no stored value", row.name)
                })?),
                other => {
                    return Err(anyhow::anyhow!(
                        "secret '{}' uses backend '{other}', not resolvable on this host yet",
                        row.name
                    ));
                }
            };
            Ok(SecretReply { value, found: true })
        }
        SecretOp::Set {
            name,
            value,
            description,
        } => {
            upsert(conn, name, BACKEND_INLINE, "", description.as_deref())?;
            write_inline_value(conn, name, value)?;
            Ok(SecretReply::default())
        }
        SecretOp::Exists { name } => Ok(SecretReply {
            value: None,
            found: get(conn, name)?.is_some(),
        }),
        SecretOp::Delete { name } => Ok(SecretReply {
            value: None,
            found: delete(conn, name)?,
        }),
    }
}

/// Run a plugin secrets op on core's single shared pooled connection — the entry
/// the loader binds into each plugin's `set_secret_op` channel.
pub fn exec_secret_op_pooled(op: &plugin_abi::SecretOp) -> Result<plugin_abi::SecretReply> {
    crate::pool::with_pooled_or_open(|conn| exec_secret_op(conn, op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn upsert_and_get_round_trip() {
        let conn = test_conn();
        assert!(get(&conn, "github_token").unwrap().is_none());

        let created = upsert(
            &conn,
            "github_token",
            "inline",
            "",
            Some("PAT for releases"),
        )
        .unwrap();
        assert!(created, "first insert should report created");

        let r = get(&conn, "github_token").unwrap().unwrap();
        assert_eq!(r.name, "github_token");
        assert_eq!(r.backend, "inline");
        assert_eq!(r.description.as_deref(), Some("PAT for releases"));

        // Update — created should be false now.
        let created = upsert(&conn, "github_token", "inline", "", Some("rotated")).unwrap();
        assert!(!created);
        let r = get(&conn, "github_token").unwrap().unwrap();
        assert_eq!(r.description.as_deref(), Some("rotated"));
    }

    #[test]
    fn list_returns_sorted() {
        let conn = test_conn();
        upsert(&conn, "zeta", "inline", "", None).unwrap();
        upsert(&conn, "alpha", "inline", "", None).unwrap();
        upsert(&conn, "mike", "inline", "", None).unwrap();
        let names: Vec<_> = list(&conn).unwrap().into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zeta"]);
    }

    #[test]
    fn delete_removes_inline_value_too() {
        let conn = test_conn();
        upsert(&conn, "k", "inline", "", None).unwrap();
        write_inline_value(&conn, "k", "v").unwrap();
        assert_eq!(read_inline_value(&conn, "k").unwrap().as_deref(), Some("v"));

        let removed = delete(&conn, "k").unwrap();
        assert!(removed);
        assert!(get(&conn, "k").unwrap().is_none());
        assert!(read_inline_value(&conn, "k").unwrap().is_none());
    }

    #[test]
    fn list_provider_instances_groups_by_instance() {
        let conn = test_conn();
        upsert(&conn, "proxmox.delta.api_url", "inline", "", None).unwrap();
        write_inline_value(&conn, "proxmox.delta.api_url", "https://delta:8006").unwrap();
        upsert(&conn, "proxmox.delta.api_token", "inline", "", None).unwrap();
        write_inline_value(&conn, "proxmox.delta.api_token", "tok-a").unwrap();
        upsert(&conn, "proxmox.lab.api_url", "inline", "", None).unwrap();
        write_inline_value(&conn, "proxmox.lab.api_url", "https://lab:8006").unwrap();
        upsert(&conn, "unrelated", "inline", "", None).unwrap();
        write_inline_value(&conn, "unrelated", "x").unwrap();

        let instances = list_provider_instances(&conn, "proxmox").unwrap();
        assert_eq!(instances.len(), 2);
        let delta = &instances.iter().find(|(i, _)| i == "delta").unwrap().1;
        assert_eq!(
            delta.get("api_url").map(String::as_str),
            Some("https://delta:8006")
        );
        assert_eq!(delta.get("api_token").map(String::as_str), Some("tok-a"));
        let lab = &instances.iter().find(|(i, _)| i == "lab").unwrap().1;
        assert_eq!(
            lab.get("api_url").map(String::as_str),
            Some("https://lab:8006")
        );
        assert!(lab.get("api_token").is_none());
    }

    #[test]
    fn list_provider_instances_skips_flat_legacy_keys() {
        let conn = test_conn();
        // `github_token` (no dots) must not be misread as a github instance.
        upsert(&conn, "github_token", "inline", "", None).unwrap();
        write_inline_value(&conn, "github_token", "tok").unwrap();
        assert!(list_provider_instances(&conn, "github").unwrap().is_empty());
        assert!(
            list_provider_instances(&conn, "github_token")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delete_missing_returns_false() {
        let conn = test_conn();
        assert!(!delete(&conn, "nope").unwrap());
    }
}
