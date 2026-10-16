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
//! A plugin persists the [`SecretRef`] (an opaque name/URL handle), not the
//! value, and resolves it at use time via [`ResolveSecret::resolve`].
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

pub use utils::secret::{Secret, SecretRef};

pub trait ResolveSecret {
    fn resolve(&self) -> Result<Secret<String>>;
}

impl ResolveSecret for SecretRef {
    fn resolve(&self) -> Result<Secret<String>> {
        get_required(self.as_str())
    }
}

/// Build a `<provider>.<instance>.<field>` secret name. Matches the convention
/// `secrets::list_provider_instances` enumerates, so secrets a plugin writes
/// this way are discoverable per instance.
pub fn scoped_name(provider: &str, instance: &str, field: &str) -> Result<SecretRef> {
    SecretRef::parse(format!("{provider}.{instance}.{field}")).map_err(|e| anyhow!(e))
}

/// Store `value` under `name` in the inline backend, creating or replacing it.
/// Returns a [`SecretRef`] the caller persists in place of the value.
///
/// Backend-agnostic by design: this is the inline path today; when per-secret
/// backends land, an overload will take a backend/policy and the return type is
/// unchanged, so call sites don't move.
pub fn set(name: &str, value: &str, description: Option<&str>) -> Result<SecretRef> {
    let secret_ref = SecretRef::parse(name).map_err(|e| anyhow!(e))?;
    secret_op(&SecretOp::Set {
        name: name.to_string(),
        value: value.to_string(),
        description: description.map(str::to_string),
    })?;
    Ok(secret_ref)
}

/// Resolve `name` to its value, or `None` if no such secret is registered.
/// Mirrors `auth::secrets::get_secret` resolution without pulling the auth
/// crate: inline resolves locally; any other backend errors until its
/// integration is loaded.
pub fn get(name: &str) -> Result<Option<Secret<String>>> {
    // Core performs the backend resolution (inline decrypt; external backends
    // error) on its pooled connection and returns the resolved value.
    Ok(secret_op(&SecretOp::Get {
        name: name.to_string(),
    })?
    .value
    .map(Secret::new))
}

/// Resolve `name`, erroring if it isn't registered.
pub fn get_required(name: &str) -> Result<Secret<String>> {
    get(name)?.ok_or_else(|| anyhow!("no secret named '{name}'"))
}

pub fn resolve_scoped(provider: &str, instance: &str, field: &str) -> Result<Secret<String>> {
    let name = scoped_name(provider, instance, field)?;
    get(name.as_str())?.ok_or_else(|| {
        MissingSecret {
            name: name.as_str().to_string(),
        }
        .into()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSecret {
    pub name: String,
}

impl std::fmt::Display for MissingSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no secret named '{}' in the secrets store", self.name)
    }
}

impl std::error::Error for MissingSecret {}

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
            scoped_name("proxmox", "host-d", "token_secret")
                .unwrap()
                .as_str(),
            "proxmox.host-d.token_secret"
        );
        assert!(scoped_name("proxmox", "", "token").is_err());
        assert!(scoped_name("proxmox", "a b", "token").is_err());
    }

    #[test]
    fn missing_secret_names_the_scoped_secret() {
        let e: anyhow::Error = MissingSecret {
            name: scoped_name("gitea", "main", "token")
                .unwrap()
                .as_str()
                .to_string(),
        }
        .into();
        assert_eq!(
            e.downcast_ref::<MissingSecret>().map(|m| m.name.as_str()),
            Some("gitea.main.token")
        );
    }
}
