//! Secrets domain — host-level named secrets with pluggable backends.
//!
//! v1 surface: `secret.list`, `secret.get`, `secret.set`, `secret.delete`,
//! `secret.backends`. The only backend in v1 is `inline` (value stored in the
//! SQLCipher-encrypted orca.db). v2 plan adds 1Password / Bitwarden / OS
//! keychain backends as separate integration crates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Shared types ────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct BackendInfo {
    pub kind: String,
    pub supports_store: bool,
}

// ── secret.list ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListReport {
    pub secrets: Vec<SecretEntry>,
}

// ── secret.get ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetArgs {
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetReport {
    pub name: String,
    pub backend: String,
    pub value: String,
}

// ── secret.set ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
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
}

fn default_inline() -> String {
    "inline".into()
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretMutationReport {
    pub name: String,
    pub backend: String,
    pub created: bool,
}

// ── secret.delete ───────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteArgs {
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteReport {
    pub name: String,
    pub removed: bool,
}

// ── secret.backends ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendsReport {
    pub backends: Vec<BackendInfo>,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn secrets_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::secrets::SecretsService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::secrets::SecretsService>>()
}

/// List configured secrets (names + backends + metadata). Never returns values.
#[orca_tool(domain = "secret", verb = "list")]
async fn secret_list(
    _args: SecretListArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SecretListReport> {
    let secrets = secrets_svc(ctx)?.list().await?;
    Ok(SecretListReport { secrets })
}

/// [SENSITIVE] Fetch a secret value by name. Resolves via the configured backend.
#[orca_tool(domain = "secret", verb = "get")]
async fn secret_get(
    args: SecretGetArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SecretGetReport> {
    let (backend, value) = secrets_svc(ctx)?.get(&args.name).await?;
    Ok(SecretGetReport {
        name: args.name,
        backend,
        value,
    })
}

/// [MUTATES STATE] Create or update a secret. For 'inline' backend, `value` is required;
/// for external backends, `ref_path` is required (e.g. 'op://Vault/Item/field').
#[orca_tool(domain = "secret", verb = "set")]
async fn secret_set(
    args: SecretSetArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    secrets_svc(ctx)?.set(args).await
}

/// [MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends
/// only the orca registration is removed (the upstream vault is untouched).
#[orca_tool(domain = "secret", verb = "delete")]
async fn secret_delete(
    args: SecretDeleteArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SecretDeleteReport> {
    let removed = secrets_svc(ctx)?.delete(&args.name).await?;
    Ok(SecretDeleteReport {
        name: args.name,
        removed,
    })
}

/// List backend kinds available on this host (lets the UI render a backend picker).
#[orca_tool(domain = "secret", verb = "backends")]
async fn secret_backends(
    _args: SecretBackendsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SecretBackendsReport> {
    let backends = secrets_svc(ctx)?.backends().await;
    Ok(SecretBackendsReport { backends })
}
