//! Secrets domain — host-level named secrets with pluggable backends.
//!
//! v1 surface: `secret.list`, `secret.get`, `secret.set`, `secret.delete`,
//! `secret.backends`. The only backend in v1 is `inline` (value stored in the
//! SQLCipher-encrypted orca.db). v2 plan adds 1Password / Bitwarden / OS
//! keychain backends as separate integration crates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

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

pub struct SecretList;
impl OrcaToolDef for SecretList {
    const NAME: &'static str = "secret.list";
    const DESCRIPTION: &'static str =
        "List configured secrets (names + backends + metadata). Never returns values.";
    type Args = SecretListArgs;
    type Output = SecretListReport;
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

pub struct SecretGet;
impl OrcaToolDef for SecretGet {
    const NAME: &'static str = "secret.get";
    const DESCRIPTION: &'static str =
        "[SENSITIVE] Fetch a secret value by name. Resolves via the configured backend.";
    type Args = SecretGetArgs;
    type Output = SecretGetReport;
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

pub struct SecretSet;
impl OrcaToolDef for SecretSet {
    const NAME: &'static str = "secret.set";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Create or update a secret. For 'inline' backend, `value` is required; \
         for external backends, `ref_path` is required (e.g. 'op://Vault/Item/field').";
    type Args = SecretSetArgs;
    type Output = SecretMutationReport;
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

pub struct SecretDelete;
impl OrcaToolDef for SecretDelete {
    const NAME: &'static str = "secret.delete";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends \
         only the orca registration is removed (the upstream vault is untouched).";
    type Args = SecretDeleteArgs;
    type Output = SecretDeleteReport;
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

pub struct SecretBackends;
impl OrcaToolDef for SecretBackends {
    const NAME: &'static str = "secret.backends";
    const DESCRIPTION: &'static str =
        "List backend kinds available on this host (lets the UI render a backend picker).";
    type Args = SecretBackendsArgs;
    type Output = SecretBackendsReport;
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::secrets::SecretsService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn SecretsService>> {
        ctx.service::<Arc<dyn SecretsService>>()
    }

    #[async_trait]
    impl OrcaTool for SecretList {
        async fn run(_a: SecretListArgs, ctx: &ToolCtx) -> Result<SecretListReport> {
            let secrets = svc(ctx)?.list().await?;
            Ok(SecretListReport { secrets })
        }
    }

    #[async_trait]
    impl OrcaTool for SecretGet {
        async fn run(a: SecretGetArgs, ctx: &ToolCtx) -> Result<SecretGetReport> {
            let (backend, value) = svc(ctx)?.get(&a.name).await?;
            Ok(SecretGetReport {
                name: a.name,
                backend,
                value,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for SecretSet {
        async fn run(a: SecretSetArgs, ctx: &ToolCtx) -> Result<SecretMutationReport> {
            svc(ctx)?.set(a).await
        }
    }

    #[async_trait]
    impl OrcaTool for SecretDelete {
        async fn run(a: SecretDeleteArgs, ctx: &ToolCtx) -> Result<SecretDeleteReport> {
            let removed = svc(ctx)?.delete(&a.name).await?;
            Ok(SecretDeleteReport {
                name: a.name,
                removed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for SecretBackends {
        async fn run(_a: SecretBackendsArgs, ctx: &ToolCtx) -> Result<SecretBackendsReport> {
            let backends = svc(ctx)?.backends().await;
            Ok(SecretBackendsReport { backends })
        }
    }
}
