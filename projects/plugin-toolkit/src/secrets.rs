//! Plugin-facing secrets facade — the abstract secrets domain, backend-agnostic.
//!
//! A plugin stores and resolves secrets **by name** and never learns where the
//! value lives. Today the only backend is `inline` (value in the SQLCipher-
//! encrypted orca.db); the roadmap adds 1Password / Bitwarden / Vaultwarden and
//! an internal store that links an external item while keeping a secure offline
//! copy — all selectable per-secret. Because callers here touch only
//! [`set`]/[`get`]/[`delete`], none of that reaches plugin code: the same three
//! calls work whatever backend a secret is bound to
//! ([[secrets-backend-agnostic-per-secret]], [[plugins-use-abstract-secrets-domain]]).
//!
//! Sensitive values (a PVE token, an API key) must be written here — never into
//! a plaintext column on a plugin's own table ([[runtime-least-privilege-not-root]]).
//! A plugin persists the [`SecretRef`] (a name + backend), not the value, and
//! resolves it at use time.
//!
//! Naming: multi-instance secrets follow `<provider>.<instance>.<field>` — build
//! the name with [`scoped_name`] so a plugin's secrets stay grouped and never
//! collide with another provider's ([[secrets-and-credential-surfaces]]).

use anyhow::{Result, anyhow};

use crate::runtime::open_db;

/// The `inline` backend: value stored in the encrypted orca.db. The one backend
/// resolvable on every host; the offline copy the internal store keeps for a
/// linked external secret also resolves through it.
pub const BACKEND_INLINE: &str = "inline";

/// A handle to a stored secret: its name and the backend it's bound to. A plugin
/// persists this (e.g. on its endpoint row) instead of the raw value, and calls
/// [`SecretRef::resolve`] when it needs the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub name: String,
    pub backend: String,
}

impl SecretRef {
    /// Resolve this reference to its value via the bound backend.
    pub fn resolve(&self) -> Result<String> {
        get_required(&self.name)
    }
}

/// Build a `<provider>.<instance>.<field>` secret name. Matches the convention
/// `db::secrets::list_provider_instances` enumerates, so secrets a plugin writes
/// this way are discoverable per instance.
pub fn scoped_name(provider: &str, instance: &str, field: &str) -> String {
    format!("{provider}.{instance}.{field}")
}

/// Store `value` under `name` in the inline backend, creating or replacing it.
/// Returns a [`SecretRef`] the caller persists in place of the value.
///
/// Backend-agnostic by design: this is the inline path today; when per-secret
/// backends land, an overload will take a backend/policy and the return type is
/// unchanged, so call sites don't move.
pub fn set(name: &str, value: &str, description: Option<&str>) -> Result<SecretRef> {
    let conn = open_db()?;
    db::secrets::upsert(&conn, name, BACKEND_INLINE, "", description)?;
    db::secrets::write_inline_value(&conn, name, value)?;
    Ok(SecretRef {
        name: name.to_string(),
        backend: BACKEND_INLINE.to_string(),
    })
}

/// Resolve `name` to its value, or `None` if no such secret is registered.
/// Mirrors `auth::secrets::get_secret` resolution without pulling the auth
/// crate: inline resolves locally; any other backend errors until its
/// integration is loaded.
pub fn get(name: &str) -> Result<Option<String>> {
    let conn = open_db()?;
    let Some(row) = db::secrets::get(&conn, name)? else {
        return Ok(None);
    };
    match row.backend.as_str() {
        BACKEND_INLINE => Ok(Some(
            db::secrets::read_inline_value(&conn, &row.name)?
                .ok_or_else(|| anyhow!("inline secret '{}' has no stored value", row.name))?,
        )),
        other => Err(anyhow!(
            "secret '{}' uses backend '{other}', not resolvable on this host yet",
            row.name
        )),
    }
}

/// Resolve `name`, erroring if it isn't registered.
pub fn get_required(name: &str) -> Result<String> {
    get(name)?.ok_or_else(|| anyhow!("no secret named '{name}'"))
}

/// True if a secret with this name is registered.
pub fn exists(name: &str) -> Result<bool> {
    let conn = open_db()?;
    Ok(db::secrets::get(&conn, name)?.is_some())
}

/// Remove a secret. For inline the value is zeroed; for external backends only
/// the orca registration is dropped (the upstream vault is untouched). Returns
/// whether anything was removed.
pub fn delete(name: &str) -> Result<bool> {
    let conn = open_db()?;
    db::secrets::delete(&conn, name)
}
