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

use crate::abi::SecretOp;
use crate::runtime::secret_op;

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
    secret_op(&SecretOp::Set {
        name: name.to_string(),
        value: value.to_string(),
        description: description.map(str::to_string),
    })?;
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
    // Core performs the backend resolution (inline decrypt; external backends
    // error) on its pooled connection and returns the resolved value.
    Ok(secret_op(&SecretOp::Get {
        name: name.to_string(),
    })?
    .value)
}

/// Resolve `name`, erroring if it isn't registered.
pub fn get_required(name: &str) -> Result<String> {
    get(name)?.ok_or_else(|| anyhow!("no secret named '{name}'"))
}

/// Resolve a `#[secret]` endpoint field **secure-first**: prefer the abstract
/// secrets domain (`<provider>.<instance>.<field>`), falling back to an inline
/// plaintext value only when the domain has none, erroring if neither is
/// present.
///
/// This is the one accessor every endpoint plugin uses to read a secret column.
/// After the plugin's bootstrap moves the value into the domain and clears the
/// plaintext column, the domain wins and the empty column is ignored — so
/// plugins converge on least-privilege without hand-rolling the three-branch
/// choice each ([[runtime-least-privilege-not-root]],
/// [[plugins-use-abstract-secrets-domain]]).
pub fn resolve_scoped(
    provider: &str,
    instance: &str,
    field: &str,
    inline_fallback: Option<&str>,
) -> Result<String> {
    let domain = get(&scoped_name(provider, instance, field))?;
    pick_secret(domain, inline_fallback).ok_or_else(|| {
        anyhow!(
            "{provider} endpoint '{instance}' has no {field} \
             (neither in the secrets domain nor inline)"
        )
    })
}

/// Pure secure-first decision, split out from [`resolve_scoped`] so the 3-way
/// choice is unit-testable without a DB: a domain value always wins; else a
/// **non-empty** inline fallback; else `None` (caller turns that into an error).
fn pick_secret(domain: Option<String>, inline_fallback: Option<&str>) -> Option<String> {
    domain.or_else(|| {
        inline_fallback
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// True if a secret with this name is registered.
pub fn exists(name: &str) -> Result<bool> {
    Ok(secret_op(&SecretOp::Exists {
        name: name.to_string(),
    })?
    .found)
}

/// Remove a secret. For inline the value is zeroed; for external backends only
/// the orca registration is dropped (the upstream vault is untouched). Returns
/// whether anything was removed.
pub fn delete(name: &str) -> Result<bool> {
    Ok(secret_op(&SecretOp::Delete {
        name: name.to_string(),
    })?
    .found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_name_joins_provider_instance_field() {
        assert_eq!(
            scoped_name("proxmox", "host-d", "token_secret"),
            "proxmox.host-d.token_secret"
        );
    }

    #[test]
    fn pick_secret_domain_wins_over_inline() {
        assert_eq!(
            pick_secret(Some("from-domain".into()), Some("from-column")),
            Some("from-domain".to_string())
        );
    }

    #[test]
    fn pick_secret_falls_back_to_nonempty_inline() {
        assert_eq!(
            pick_secret(None, Some("from-column")),
            Some("from-column".to_string())
        );
    }

    #[test]
    fn pick_secret_ignores_empty_inline() {
        // Post-bootstrap: the plaintext column is cleared to "" — it must not
        // count as a usable secret, so resolution errors rather than authing
        // with an empty token.
        assert_eq!(pick_secret(None, Some("")), None);
    }

    #[test]
    fn pick_secret_none_when_neither_present() {
        assert_eq!(pick_secret(None, None), None);
    }
}
