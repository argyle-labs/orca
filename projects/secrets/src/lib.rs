//! Secrets — host-level secret metadata with pluggable backends.
//!
//! The `secrets` table records `{name, backend, ref_path, description}`. For
//! the `inline` backend, the actual value lives in `settings` under the
//! `secrets.{name}` prefix (so existing `settings::secret_*` helpers and the
//! `auth.login` path stay interoperable). For external backends (op, bw,
//! keychain, …) the value is fetched on demand and `ref_path` is the vendor-
//! specific address.

pub mod plugin_columns;

use anyhow::Result;
use db::settings;
use rusqlite::Connection;

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

/// Mesh storage gate. Refuses secret writes when this host has joined a
/// mesh but is not flagged secure (mesh_self.self_secure = 0). Hosts that
/// have never run `orca system join` (no mesh_self row) bypass
/// the gate so non-mesh workflows still work.
pub fn ensure_write_allowed(conn: &Connection) -> Result<()> {
    use rusqlite::OptionalExtension;
    let v: Option<i64> = conn
        .query_row("SELECT self_secure FROM mesh_self WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    match v {
        None => Ok(()), // host hasn't joined a mesh — no gating
        Some(1) => Ok(()),
        Some(_) => anyhow::bail!(
            "secrets storage disabled on this host — run \
             `orca system mesh update --action settings --self-secure true` to enable"
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
    ensure_write_allowed(conn)?;
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
    clear_restore_state(conn, name)?;
    Ok(!existed)
}

/// Delete the metadata row and (for inline) any stored value. Returns true if anything was removed.
pub fn delete(conn: &Connection, name: &str) -> Result<bool> {
    ensure_write_allowed(conn)?;
    // Look up the record first so we know whether to also clean the inline value.
    let record = get(conn, name)?;
    let mut removed = false;
    if let Some(r) = &record {
        let n = conn.execute(
            "DELETE FROM secrets WHERE name = ?1",
            rusqlite::params![name],
        )?;
        removed = n > 0;
        conn.execute(
            "DELETE FROM secret_grants WHERE secret_name = ?1",
            rusqlite::params![name],
        )?;
        if r.backend == "inline" {
            // Best-effort: ignore "not present" since the metadata is the
            // source of truth for whether the secret existed.
            let _ = settings::secret_delete(conn, name)?;
        }
        clear_restore_state(conn, name)?;
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
    ensure_write_allowed(conn)?;
    settings::secret_set(conn, name, value)
}

fn clear_restore_state(conn: &Connection, name: &str) -> Result<()> {
    settings::secret_previous_delete(conn, name)?;
    settings::secret_restore_marker_delete(conn, name)?;
    Ok(())
}

pub fn put_inline_value(conn: &Connection, name: &str, value: &str) -> Result<()> {
    ensure_write_allowed(conn)?;
    match read_inline_value(conn, name)? {
        Some(old) => settings::secret_previous_set(conn, name, &old)?,
        None => {
            settings::secret_previous_delete(conn, name)?;
        }
    }
    settings::secret_set(conn, name, value)
}

pub fn restore_inline_value(conn: &Connection, name: &str) -> Result<()> {
    ensure_write_allowed(conn)?;
    let previous = settings::secret_previous_get(conn, name)?
        .ok_or_else(|| anyhow::anyhow!("inline secret '{name}' has no previous value"))?;
    settings::secret_previous_delete(conn, name)?;
    settings::secret_set(conn, name, &previous)
}

fn restore_marker(record: &SecretRecord) -> String {
    format!("{}\n{}", record.backend, record.ref_path)
}

pub fn arm_external_restore(conn: &Connection, record: &SecretRecord) -> Result<()> {
    ensure_write_allowed(conn)?;
    settings::secret_restore_marker_set(conn, &record.name, &restore_marker(record))
}

pub fn ensure_external_restore_armed(conn: &Connection, record: &SecretRecord) -> Result<()> {
    ensure_write_allowed(conn)?;
    if settings::secret_restore_marker_get(conn, &record.name)?.as_deref()
        != Some(restore_marker(record).as_str())
    {
        anyhow::bail!(
            "secret '{}' has no previous value to restore on backend '{}'",
            record.name,
            record.backend
        );
    }
    Ok(())
}

pub fn disarm_external_restore(conn: &Connection, name: &str) -> Result<()> {
    settings::secret_restore_marker_delete(conn, name)?;
    Ok(())
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
// Invoked by the loader's `secret.op` capability handler when a subprocess
// plugin sends a `SecretOp` over `plugin-proto`. `plugin_toolkit::secrets`
// otherwise opens its OWN connection to run this same SQL, racing the daemon's
// on the WAL/shm index (SHMOPEN 5898). Core owns the crypto + the tables, so the
// whole op runs here.

/// The inline backend tag — a secret whose value lives in orca's own encrypted
/// store (mirrors `plugin_toolkit::secrets::BACKEND_INLINE`).
pub(crate) const BACKEND_INLINE: &str = "inline";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Core,
    Plugin(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAccessDenied {
    pub plugin: String,
    pub name: String,
    pub op: &'static str,
}

impl std::fmt::Display for SecretAccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin '{}' may not secret.op/{} on '{}': outside its namespace and not granted",
            self.plugin, self.op, self.name
        )
    }
}

impl std::error::Error for SecretAccessDenied {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretGrant {
    pub holder: String,
    pub secret_name: String,
    pub granted_by: String,
    pub created_at: String,
}

fn valid_principal(plugin: &str) -> bool {
    db::plugin_tables::validate_principal(plugin).is_ok()
}

fn in_namespace(name: &str, plugin: &str) -> bool {
    name == plugin
        || name
            .strip_prefix(plugin)
            .is_some_and(|rest| rest.starts_with('.'))
}

fn has_grant(conn: &Connection, holder: &str, name: &str) -> Result<bool> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT 1 FROM secret_grants WHERE holder = ?1 AND secret_name = ?2",
            rusqlite::params![holder, name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn authorize(conn: &Connection, principal: &Principal, op: &plugin_abi::SecretOp) -> Result<()> {
    use plugin_abi::SecretOp;
    let Principal::Plugin(plugin) = principal else {
        return Ok(());
    };
    let name = op.name();
    if !valid_principal(plugin) {
        tracing::warn!(plugin = %plugin, secret = %name, op = op.kind(), "secret access denied: invalid principal");
        return Err(SecretAccessDenied {
            plugin: plugin.clone(),
            name: name.to_string(),
            op: op.kind(),
        }
        .into());
    }
    if in_namespace(name, plugin) {
        return Ok(());
    }
    let readonly = matches!(op, SecretOp::Get { .. } | SecretOp::Exists { .. });
    if readonly && has_grant(conn, plugin, name)? {
        return Ok(());
    }
    tracing::warn!(plugin = %plugin, secret = %name, op = op.kind(), "secret access denied");
    Err(SecretAccessDenied {
        plugin: plugin.clone(),
        name: name.to_string(),
        op: op.kind(),
    }
    .into())
}

pub fn grant(conn: &Connection, holder: &str, secret_name: &str, granted_by: &str) -> Result<bool> {
    let n = conn.execute(
        "INSERT INTO secret_grants (holder, secret_name, granted_by) VALUES (?1, ?2, ?3)
         ON CONFLICT(holder, secret_name) DO NOTHING",
        rusqlite::params![holder, secret_name, granted_by],
    )?;
    Ok(n > 0)
}

pub fn grant_checked(
    conn: &Connection,
    holder: &str,
    secret_name: &str,
    granted_by: &str,
) -> Result<bool> {
    if !valid_principal(holder) {
        anyhow::bail!("invalid holder plugin id '{holder}'");
    }
    if secret_name.is_empty()
        || secret_name.chars().any(|c| {
            matches!(c, '*' | '?' | '%' | '[' | ']') || c.is_whitespace() || c.is_control()
        })
    {
        anyhow::bail!("invalid secret name '{secret_name}': grants name one exact secret");
    }
    if in_namespace(secret_name, holder) {
        anyhow::bail!("'{holder}' already owns '{secret_name}'; a grant would be redundant");
    }
    if get(conn, secret_name)?.is_none() {
        anyhow::bail!("secret '{secret_name}' does not exist");
    }
    grant(conn, holder, secret_name, granted_by)
}

pub fn revoke_holder(conn: &Connection, holder: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("DELETE FROM secret_grants WHERE holder = ?1 RETURNING secret_name")?;
    let rows = stmt.query_map([holder], |row| row.get(0))?;
    let mut names = rows.collect::<rusqlite::Result<Vec<String>>>()?;
    names.sort();
    Ok(names)
}

pub fn revoke(conn: &Connection, holder: &str, secret_name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM secret_grants WHERE holder = ?1 AND secret_name = ?2",
        rusqlite::params![holder, secret_name],
    )?;
    Ok(n > 0)
}

pub fn list_grants(conn: &Connection) -> Result<Vec<SecretGrant>> {
    let mut stmt = conn.prepare(
        "SELECT holder, secret_name, granted_by, created_at FROM secret_grants
         ORDER BY holder, secret_name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SecretGrant {
            holder: row.get(0)?,
            secret_name: row.get(1)?,
            granted_by: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn exec_secret_op(
    conn: &Connection,
    principal: &Principal,
    op: &plugin_abi::SecretOp,
) -> Result<plugin_abi::SecretReply> {
    exec_secret_op_via(principal, op, |f| f(conn))
}

enum Prepared {
    Reply(plugin_abi::SecretReply),
    External(SecretRecord),
}

fn exec_secret_op_via(
    principal: &Principal,
    op: &plugin_abi::SecretOp,
    with_conn: impl FnOnce(&dyn Fn(&Connection) -> Result<Prepared>) -> Result<Prepared>,
) -> Result<plugin_abi::SecretReply> {
    let prepared = with_conn(&|conn| {
        authorize(conn, principal, op)?;
        prepare_secret_op(conn, op)
    })?;
    match prepared {
        Prepared::Reply(reply) => Ok(reply),
        Prepared::External(row) => {
            let value = utils::reactor::off_async_worker(|| {
                utils::reactor::block_on(contract::secrets_backend::resolve(
                    &row.backend,
                    &row.ref_path,
                ))
            })
            .map_err(|e| {
                anyhow::anyhow!("secret '{}' (backend '{}'): {e:#}", row.name, row.backend)
            })?;
            Ok(plugin_abi::SecretReply {
                value: Some(value),
                found: true,
            })
        }
    }
}

fn prepare_secret_op(conn: &Connection, op: &plugin_abi::SecretOp) -> Result<Prepared> {
    use plugin_abi::{SecretOp, SecretReply};
    let reply = match op {
        SecretOp::Get { name } => {
            let Some(row) = get(conn, name)? else {
                return Ok(Prepared::Reply(SecretReply::default()));
            };
            if row.backend != BACKEND_INLINE {
                return Ok(Prepared::External(row));
            }
            let value = read_inline_value(conn, &row.name)?.ok_or_else(|| {
                anyhow::anyhow!("inline secret '{}' has no stored value", row.name)
            })?;
            SecretReply {
                value: Some(value),
                found: true,
            }
        }
        SecretOp::Set {
            name,
            value,
            description,
        } => {
            upsert(conn, name, BACKEND_INLINE, "", description.as_deref())?;
            write_inline_value(conn, name, value)?;
            SecretReply::default()
        }
        SecretOp::Exists { name } => SecretReply {
            value: None,
            found: get(conn, name)?.is_some(),
        },
        SecretOp::Delete { name } => SecretReply {
            value: None,
            found: delete(conn, name)?,
        },
    };
    Ok(Prepared::Reply(reply))
}

pub fn ensure_backend_registered(backend: &str) -> Result<()> {
    if backend == BACKEND_INLINE {
        return Ok(());
    }
    let mut registered: Vec<String> = contract::secrets_backend::providers()
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    if registered.iter().any(|n| n == backend) {
        return Ok(());
    }
    registered.sort();
    registered.insert(0, BACKEND_INLINE.to_string());
    Err(anyhow::anyhow!(
        "secrets backend '{backend}' is not registered on this host; registered: {}",
        registered.join(", ")
    ))
}

pub fn exec_secret_op_pooled(
    principal: &Principal,
    op: &plugin_abi::SecretOp,
) -> Result<plugin_abi::SecretReply> {
    exec_secret_op_via(principal, op, |f| db::pool::with_pooled_or_open(f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::testing::test_conn;

    #[test]
    fn restore_applies_previous_once_then_fails() {
        let conn = test_conn();
        upsert(&conn, "app.pw", "inline", "", None).unwrap();
        write_inline_value(&conn, "app.pw", "one").unwrap();
        put_inline_value(&conn, "app.pw", "two").unwrap();
        assert_eq!(
            read_inline_value(&conn, "app.pw").unwrap().as_deref(),
            Some("two")
        );
        restore_inline_value(&conn, "app.pw").unwrap();
        assert_eq!(
            read_inline_value(&conn, "app.pw").unwrap().as_deref(),
            Some("one")
        );
        let err = restore_inline_value(&conn, "app.pw").unwrap_err();
        assert!(err.to_string().contains("no previous value"), "{err}");
        assert_eq!(
            read_inline_value(&conn, "app.pw").unwrap().as_deref(),
            Some("one")
        );
        put_inline_value(&conn, "app.pw", "three").unwrap();
        assert!(delete(&conn, "app.pw").unwrap());
        assert!(
            settings::secret_previous_get(&conn, "app.pw")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn upsert_clears_the_previous_value() {
        let conn = test_conn();
        upsert(&conn, "app.pw", "inline", "", None).unwrap();
        write_inline_value(&conn, "app.pw", "one").unwrap();
        put_inline_value(&conn, "app.pw", "two").unwrap();
        upsert(&conn, "app.pw", "inline", "", None).unwrap();
        assert!(
            settings::secret_previous_get(&conn, "app.pw")
                .unwrap()
                .is_none()
        );
        assert!(restore_inline_value(&conn, "app.pw").is_err());
    }

    #[test]
    fn writes_are_refused_on_a_non_secure_mesh_member() {
        let conn = test_conn();
        upsert(&conn, "app.pw", "inline", "", None).unwrap();
        write_inline_value(&conn, "app.pw", "one").unwrap();
        put_inline_value(&conn, "app.pw", "two").unwrap();
        conn.execute(
            "INSERT INTO mesh_self (id, self_secure, set_at) VALUES (1, 0, 0)",
            [],
        )
        .unwrap();
        assert!(ensure_write_allowed(&conn).is_err());
        assert!(put_inline_value(&conn, "app.pw", "x").is_err());
        assert!(restore_inline_value(&conn, "app.pw").is_err());
        assert!(upsert(&conn, "app.pw", "inline", "", None).is_err());
        assert!(delete(&conn, "app.pw").is_err());
        assert!(get(&conn, "app.pw").unwrap().is_some());
        assert_eq!(
            read_inline_value(&conn, "app.pw").unwrap().as_deref(),
            Some("two")
        );
    }

    #[test]
    fn external_restore_is_armed_once_by_put_and_cleared_by_upsert_and_delete() {
        let conn = test_conn();
        upsert(&conn, "ext", "fake-959", "fake://a", None).unwrap();
        let rec = get(&conn, "ext").unwrap().unwrap();
        assert!(ensure_external_restore_armed(&conn, &rec).is_err());
        arm_external_restore(&conn, &rec).unwrap();
        ensure_external_restore_armed(&conn, &rec).unwrap();
        disarm_external_restore(&conn, "ext").unwrap();
        assert!(ensure_external_restore_armed(&conn, &rec).is_err());

        arm_external_restore(&conn, &rec).unwrap();
        upsert(&conn, "ext", "fake-959", "fake://b", None).unwrap();
        assert!(ensure_external_restore_armed(&conn, &rec).is_err());
        let moved = get(&conn, "ext").unwrap().unwrap();
        arm_external_restore(&conn, &rec).unwrap();
        assert!(ensure_external_restore_armed(&conn, &moved).is_err());

        arm_external_restore(&conn, &moved).unwrap();
        assert!(delete(&conn, "ext").unwrap());
        assert!(
            settings::secret_restore_marker_get(&conn, "ext")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restore_without_a_previous_value_errors() {
        let conn = test_conn();
        upsert(&conn, "fresh", "inline", "", None).unwrap();
        put_inline_value(&conn, "fresh", "first").unwrap();
        let err = restore_inline_value(&conn, "fresh").unwrap_err();
        assert!(err.to_string().contains("no previous value"), "{err}");
    }

    #[test]
    fn checked_grant_allows_holder_read_until_revoked() {
        let conn = test_conn();
        upsert(&conn, "gitea.api.token", "inline", "", None).unwrap();
        write_inline_value(&conn, "gitea.api.token", "tok").unwrap();
        let holder = Principal::Plugin("mylar".into());
        let op = plugin_abi::SecretOp::Get {
            name: "gitea.api.token".into(),
        };
        assert!(exec_secret_op(&conn, &holder, &op).is_err());
        assert!(grant_checked(&conn, "mylar", "gitea.api.token", "alice").unwrap());
        let reply = exec_secret_op(&conn, &holder, &op).unwrap();
        assert_eq!(reply.value.as_deref(), Some("tok"));
        assert_eq!(list_grants(&conn).unwrap()[0].granted_by, "alice");
        assert!(revoke(&conn, "mylar", "gitea.api.token").unwrap());
        assert!(exec_secret_op(&conn, &holder, &op).is_err());
    }

    struct FakeBackend;

    #[async_trait::async_trait]
    impl contract::secrets_backend::SecretsBackend for FakeBackend {
        fn name(&self) -> &str {
            "fake-959"
        }

        async fn resolve(&self, ref_path: &str) -> Result<String> {
            match ref_path {
                "fake://vault/item/credential" => Ok("resolved".into()),
                _ => Err(anyhow::anyhow!("unknown ref")),
            }
        }
    }

    #[test]
    fn get_resolves_external_backend_for_granted_holder() {
        contract::secrets_backend::register_provider(std::sync::Arc::new(FakeBackend)).ok();
        let conn = test_conn();
        upsert(
            &conn,
            "pve.token",
            "fake-959",
            "fake://vault/item/credential",
            None,
        )
        .unwrap();
        let op = plugin_abi::SecretOp::Get {
            name: "pve.token".into(),
        };
        let holder = Principal::Plugin("proxmox".into());
        assert!(exec_secret_op(&conn, &holder, &op).is_err());
        assert!(grant_checked(&conn, "proxmox", "pve.token", "a").unwrap());
        let reply = exec_secret_op(&conn, &holder, &op).unwrap();
        assert_eq!(reply.value.as_deref(), Some("resolved"));
        assert!(reply.found);
        assert_eq!(read_inline_value(&conn, "pve.token").unwrap(), None);
    }

    #[test]
    fn ensure_backend_registered_rejects_unknown_and_lists_registered() {
        contract::secrets_backend::register_provider(std::sync::Arc::new(FakeBackend)).ok();
        assert!(ensure_backend_registered("inline").is_ok());
        assert!(ensure_backend_registered("fake-959").is_ok());
        let err = ensure_backend_registered("op").unwrap_err().to_string();
        assert!(err.contains("'op'"), "{err}");
        assert!(err.contains("inline") && err.contains("fake-959"), "{err}");
    }

    #[test]
    fn get_errors_for_unregistered_or_failing_backend() {
        contract::secrets_backend::register_provider(std::sync::Arc::new(FakeBackend)).ok();
        let conn = test_conn();
        upsert(&conn, "a.tok", "absent-959", "x://y", None).unwrap();
        upsert(&conn, "b.tok", "fake-959", "fake://other", None).unwrap();
        for name in ["a.tok", "b.tok"] {
            let op = plugin_abi::SecretOp::Get { name: name.into() };
            let err = exec_secret_op(&conn, &Principal::Core, &op).unwrap_err();
            assert!(format!("{err:#}").contains(name));
        }
    }

    #[tokio::test]
    async fn get_resolves_from_inside_a_runtime() {
        contract::secrets_backend::register_provider(std::sync::Arc::new(FakeBackend)).ok();
        let conn = test_conn();
        upsert(
            &conn,
            "c.tok",
            "fake-959",
            "fake://vault/item/credential",
            None,
        )
        .unwrap();
        let op = plugin_abi::SecretOp::Get {
            name: "c.tok".into(),
        };
        let reply = exec_secret_op(&conn, &Principal::Core, &op).unwrap();
        assert_eq!(reply.value.as_deref(), Some("resolved"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_resolves_from_inside_a_multi_thread_runtime() {
        contract::secrets_backend::register_provider(std::sync::Arc::new(FakeBackend)).ok();
        let conn = test_conn();
        upsert(
            &conn,
            "d.tok",
            "fake-959",
            "fake://vault/item/credential",
            None,
        )
        .unwrap();
        let op = plugin_abi::SecretOp::Get {
            name: "d.tok".into(),
        };
        let reply = exec_secret_op(&conn, &Principal::Core, &op).unwrap();
        assert_eq!(reply.value.as_deref(), Some("resolved"));
    }

    static ONE_CONN: std::sync::LazyLock<std::sync::Mutex<Connection>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(test_conn()));

    struct CallsBackIntoDb;

    #[async_trait::async_trait]
    impl contract::secrets_backend::SecretsBackend for CallsBackIntoDb {
        fn name(&self) -> &str {
            "callback-db"
        }

        async fn resolve(&self, _ref_path: &str) -> Result<String> {
            let conn = ONE_CONN.lock().unwrap();
            Ok(list(&conn)?.len().to_string())
        }
    }

    #[test]
    fn external_resolve_runs_after_the_connection_is_released() {
        contract::secrets_backend::register_provider(std::sync::Arc::new(CallsBackIntoDb)).unwrap();
        upsert(
            &ONE_CONN.lock().unwrap(),
            "cb.tok",
            "callback-db",
            "x://y",
            None,
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let op = plugin_abi::SecretOp::Get {
                name: "cb.tok".into(),
            };
            let reply = exec_secret_op_via(&Principal::Core, &op, |f| f(&ONE_CONN.lock().unwrap()));
            tx.send(reply.map(|r| r.value)).unwrap();
        });
        let value = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("resolve deadlocked holding the connection")
            .unwrap();
        assert_eq!(value.as_deref(), Some("1"));
    }

    #[test]
    fn deleting_a_secret_drops_its_grants() {
        let conn = test_conn();
        upsert(&conn, "anthropic_api_key", "inline", "", None).unwrap();
        assert!(grant_checked(&conn, "mylar", "anthropic_api_key", "a").unwrap());
        assert!(delete(&conn, "anthropic_api_key").unwrap());
        upsert(&conn, "anthropic_api_key", "inline", "", None).unwrap();
        assert!(list_grants(&conn).unwrap().is_empty());
    }

    #[test]
    fn checked_grant_refuses_missing_redundant_and_invalid() {
        let conn = test_conn();
        upsert(&conn, "gitea.api.token", "inline", "", None).unwrap();
        assert!(grant_checked(&conn, "mylar", "nope.token", "a").is_err());
        assert!(grant_checked(&conn, "gitea", "gitea.api.token", "a").is_err());
        assert!(grant_checked(&conn, "Bad Id", "gitea.api.token", "a").is_err());
        upsert(&conn, "gitea.*", "inline", "", None).unwrap();
        assert!(grant_checked(&conn, "mylar", "gitea.*", "a").is_err());
        assert!(grant_checked(&conn, "mylar", "gitea.%", "a").is_err());
        assert!(grant_checked(&conn, "mylar", "", "a").is_err());
        assert!(list_grants(&conn).unwrap().is_empty());
    }

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

    fn plugin(name: &str) -> Principal {
        Principal::Plugin(name.to_string())
    }

    fn seed(conn: &Connection, name: &str, value: &str) {
        upsert(conn, name, "inline", "", None).unwrap();
        write_inline_value(conn, name, value).unwrap();
    }

    fn get_op(name: &str) -> plugin_abi::SecretOp {
        plugin_abi::SecretOp::Get {
            name: name.to_string(),
        }
    }

    fn denied(r: Result<plugin_abi::SecretReply>) -> bool {
        r.unwrap_err()
            .downcast_ref::<SecretAccessDenied>()
            .is_some()
    }

    #[test]
    fn plugin_reads_own_namespace() {
        let conn = test_conn();
        seed(&conn, "s3.main.key", "v");
        let r = exec_secret_op(&conn, &plugin("s3"), &get_op("s3.main.key")).unwrap();
        assert_eq!(r.value.as_deref(), Some("v"));
    }

    #[test]
    fn plugin_reads_granted_ref() {
        let conn = test_conn();
        seed(&conn, "gitea.token", "t");
        assert!(grant(&conn, "zot", "gitea.token", "operator").unwrap());
        let r = exec_secret_op(&conn, &plugin("zot"), &get_op("gitea.token")).unwrap();
        assert_eq!(r.value.as_deref(), Some("t"));
        let del = plugin_abi::SecretOp::Delete {
            name: "gitea.token".to_string(),
        };
        assert!(denied(exec_secret_op(&conn, &plugin("zot"), &del)));
    }

    #[test]
    fn plugin_denied_other_plugins_secret() {
        let conn = test_conn();
        seed(&conn, "gitea.token", "t");
        assert!(denied(exec_secret_op(
            &conn,
            &plugin("zot"),
            &get_op("gitea.token")
        )));
    }

    #[test]
    fn lookalike_prefix_denied() {
        let conn = test_conn();
        seed(&conn, "s3x.main.key", "v");
        assert!(denied(exec_secret_op(
            &conn,
            &plugin("s3"),
            &get_op("s3x.main.key")
        )));
    }

    #[test]
    fn revoke_removes_access() {
        let conn = test_conn();
        seed(&conn, "gitea.token", "t");
        grant(&conn, "zot", "gitea.token", "operator").unwrap();
        assert!(revoke(&conn, "zot", "gitea.token").unwrap());
        assert!(list_grants(&conn).unwrap().is_empty());
        assert!(denied(exec_secret_op(
            &conn,
            &plugin("zot"),
            &get_op("gitea.token")
        )));
    }

    #[test]
    fn core_principal_unrestricted() {
        let conn = test_conn();
        seed(&conn, "gitea.token", "t");
        let r = exec_secret_op(&conn, &Principal::Core, &get_op("gitea.token")).unwrap();
        assert_eq!(r.value.as_deref(), Some("t"));
    }

    #[test]
    fn principal_rules_match_db() {
        assert!(valid_principal("calibre-web"));
        assert!(!valid_principal("calibre_web"));
        assert!(!valid_principal("a--b"));
    }

    #[test]
    fn invalid_principal_denied() {
        let conn = test_conn();
        seed(&conn, "gitea.home.token", "t");
        seed(&conn, ".x", "t");
        for p in ["", "gitea.home", "Gitea", "gitea.", ".", "g\u{e9}"] {
            assert!(denied(exec_secret_op(&conn, &plugin(p), &get_op(p))), "{p}");
        }
        assert!(denied(exec_secret_op(&conn, &plugin(""), &get_op(".x"))));
        assert!(denied(exec_secret_op(
            &conn,
            &plugin("gitea.home"),
            &get_op("gitea.home.token")
        )));
    }

    #[test]
    fn grant_does_not_permit_writes_or_exists_leak_beyond_grant() {
        let conn = test_conn();
        seed(&conn, "gitea.token", "t");
        grant(&conn, "zot", "gitea.token", "operator").unwrap();
        let set = plugin_abi::SecretOp::Set {
            name: "gitea.token".to_string(),
            value: "x".to_string(),
            description: None,
        };
        assert!(denied(exec_secret_op(&conn, &plugin("zot"), &set)));
        let exists = plugin_abi::SecretOp::Exists {
            name: "gitea.other".to_string(),
        };
        assert!(denied(exec_secret_op(&conn, &plugin("zot"), &exists)));
        assert!(denied(exec_secret_op(
            &conn,
            &plugin("zot"),
            &get_op("gitea.token.")
        )));
    }
}
