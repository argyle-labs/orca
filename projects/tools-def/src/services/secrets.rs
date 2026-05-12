//! `SecretsService` — host-facing API the secret tools dispatch through.
//! `SecretsBackend` — pluggable backend trait; v1 ships `inline` only,
//! v2 adds 1Password / Bitwarden / OS keychain as separate integration crates.

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_secrets::{BackendInfo, SecretEntry, SecretMutationReport, SecretSetArgs};

/// Wrapper around a fetched secret value. `Debug` is redacted so accidental
/// logging never leaks the value — callers that need the raw string must
/// `.into_inner()` (or `value.0`) explicitly.
#[derive(Clone)]
pub struct SecretValue(pub String);

impl SecretValue {
    pub fn into_inner(self) -> String {
        self.0
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "SecretValue(***{} chars***)", self.0.len())
    }
}

#[async_trait]
pub trait SecretsService: Send + Sync {
    /// All registered secrets (no values).
    async fn list(&self) -> Result<Vec<SecretEntry>>;

    /// Fetch a secret value by name. Returns `(backend_kind, value)`.
    async fn get(&self, name: &str) -> Result<(String, String)>;

    /// Create or update a secret. Behavior depends on `args.backend`:
    /// - `inline`: requires `args.value`; stores it in the encrypted DB.
    /// - external: requires `args.ref_path`; metadata only — value fetched on demand.
    async fn set(&self, args: SecretSetArgs) -> Result<SecretMutationReport>;

    /// Remove a secret. Returns true if anything was removed.
    async fn delete(&self, name: &str) -> Result<bool>;

    /// Backend kinds available on this host.
    async fn backends(&self) -> Vec<BackendInfo>;
}

/// Pluggable backend that resolves a `ref_path` to a value (read) and optionally
/// stores values (write). v1 ships `InlineBackend`; v2 adds vendor-specific
/// impls in `projects/integrations/<vendor>/`.
#[async_trait]
pub trait SecretsBackend: Send + Sync {
    /// Stable string identifier (`inline`, `op-connect`, `bitwarden`, ...).
    fn kind(&self) -> &'static str;

    /// Whether `store` is implemented. Read-only backends (env, op without write
    /// scope) return false; UI hides "edit value" for those.
    fn supports_store(&self) -> bool;

    /// Fetch the value at `ref_path`. For `inline`, `ref_path` is the secret
    /// `name` (backend takes care of looking it up).
    async fn fetch(&self, ref_path: &str) -> Result<SecretValue>;

    /// Persist `value` to the backend. Returns the canonical `ref_path` to
    /// persist alongside the metadata row.
    async fn store(&self, name: &str, value: &str) -> Result<String>;

    /// Remove the stored value (best-effort for inline; for external backends
    /// this should be a no-op or vendor-specific cleanup).
    async fn delete(&self, ref_path: &str) -> Result<()>;
}
