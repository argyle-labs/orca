//! Secrets domain — host-level named secrets with pluggable backends.
//!
//! v1 surface: `secret.list`, `secret.get`, `secret.set`, `secret.delete`,
//! `secret.backends`. The only backend in v1 is `inline` (value stored in the
//! SQLCipher-encrypted orca.db). v2 plan adds 1Password / Bitwarden / OS
//! keychain backends as separate integration crates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_tools_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

// ── Shared types ────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SecretEntry {
    pub name: String,
    /// Backend kind: "inline" (v1) | "env" | "op-connect" | "op-cli" | "bitwarden" | "keychain-macos" | "secret-service" | "wincred" (v2+).
    pub backend: String,
    /// Backend-specific reference (e.g. `op://Personal/orca-gh/token`). Empty for inline.
    pub ref_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct BackendInfo {
    pub kind: String,
    pub supports_store: bool,
}

// ── secret.list ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListReport {
    pub secrets: Vec<SecretEntry>,
}

// ── secret.get ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetReport {
    pub name: String,
    pub backend: String,
    pub value: String,
}

// ── secret.set ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SecretSetArgs {
    pub name: String,
    /// Backend kind. Defaults to "inline".
    #[serde(default = "default_inline")]
    #[cfg_attr(feature = "cli", arg(long, default_value = "inline"))]
    pub backend: String,
    /// Required for `inline`. Ignored for external backends (which use `ref_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub value: Option<String>,
    /// Required for external backends (e.g. `op://Personal/orca-gh/token`). Ignored for inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub ref_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub description: Option<String>,
    /// When set, proxy the call to the named remote peer via the pod mesh
    /// instead of writing the secret locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long, hide = true))]
    pub peer_id: Option<String>,
}

fn default_inline() -> String {
    "inline".into()
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretMutationReport {
    pub name: String,
    pub backend: String,
    pub created: bool,
}

// ── secret.delete ───────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteReport {
    pub name: String,
    pub removed: bool,
}

// ── secret.backends ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendsReport {
    pub backends: Vec<BackendInfo>,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List configured secrets (names + backends + metadata). Never returns values.
#[orca_tool(domain = "system.secret", verb = "list")]
async fn secret_list(
    _args: SecretListArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SecretListReport> {
    let secrets = ctx.service::<Arc<dyn SecretsService>>()?.list().await?;
    Ok(SecretListReport { secrets })
}

/// [SENSITIVE] Fetch a secret value by name. Resolves via the configured backend.
#[orca_tool(domain = "system.secret", verb = "detail")]
async fn secret_detail(
    args: SecretGetArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SecretGetReport> {
    let (backend, value) = ctx
        .service::<Arc<dyn SecretsService>>()?
        .get(&args.name)
        .await?;
    Ok(SecretGetReport {
        name: args.name,
        backend,
        value,
    })
}

/// [MUTATES STATE] Create or update a secret. For 'inline' backend, `value` is required;
/// for external backends, `ref_path` is required (e.g. 'op://Vault/Item/field').
/// When `peer_id` is set the secret is written on the named peer instead of locally
/// — same admin trust surface as `system.update`.
#[orca_tool(
    domain = "system.secret",
    verb = "set",
    remote_ok = true,
    peer_dispatch = true
)]
async fn secret_set(
    args: SecretSetArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    ctx.service::<Arc<dyn SecretsService>>()?.set(args).await
}

/// [MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends
/// only the orca registration is removed (the upstream vault is untouched).
#[orca_tool(domain = "system.secret", verb = "delete")]
async fn secret_delete(
    args: SecretDeleteArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SecretDeleteReport> {
    let removed = ctx
        .service::<Arc<dyn SecretsService>>()?
        .delete(&args.name)
        .await?;
    Ok(SecretDeleteReport {
        name: args.name,
        removed,
    })
}

/// List backend kinds available on this host (lets the UI render a backend picker).
#[orca_tool(domain = "system.secret", verb = "backends")]
async fn secret_backends(
    _args: SecretBackendsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SecretBackendsReport> {
    let backends = ctx.service::<Arc<dyn SecretsService>>()?.backends().await;
    Ok(SecretBackendsReport { backends })
}

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

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

/// Embedder hook — see `services::mod` doc.
pub trait ProvideSecrets {
    fn secrets(&self) -> std::sync::Arc<dyn SecretsService>;
}

/// Register a `SecretsService` into `ToolCtx`.
pub fn register_secrets(ctx: &mut orca_tool::ToolCtx, p: &impl ProvideSecrets) {
    ctx.register_service(p.secrets());
}
