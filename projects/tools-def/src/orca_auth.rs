//! Auth domain — unified surface for credential management across providers.
//!
//! Single set of ops drives Anthropic API-key storage, GitHub device-flow
//! OAuth, and Atlassian PKCE OAuth. Each provider has different mechanics
//! but the surface is the same: `auth.status`, `auth.login`, `auth.logout`.
//!
//! Why one surface for three flows: every UI (CLI, web, future native
//! clients) needs to ask "am I logged in?" and "log me out" the same way
//! regardless of which provider. The differences are confined to the
//! `AuthService` impl in the server crate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Shared rows ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct AuthProviderStatus {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
    /// True iff a credential is currently stored for this provider.
    pub configured: bool,
    /// Masked identifier (masked API key, account login, etc.) when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct AuthStatusReport {
    pub providers: Vec<AuthProviderStatus>,
}

// ── auth.status ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthStatusArgs {}

pub struct AuthStatus;
impl OrcaToolDef for AuthStatus {
    const NAME: &'static str = "auth.status";
    const DESCRIPTION: &'static str =
        "Snapshot every configured credential the host knows about (Anthropic key + OAuth tokens).";
    type Args = AuthStatusArgs;
    type Output = AuthStatusReport;
}

// ── auth.logout ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLogoutArgs {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLogoutOutput {
    pub provider: String,
    pub removed: bool,
}

pub struct AuthLogout;
impl OrcaToolDef for AuthLogout {
    const NAME: &'static str = "auth.logout";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a stored credential. `removed=false` if nothing was stored.";
    type Args = AuthLogoutArgs;
    type Output = AuthLogoutOutput;
}

// ── auth.login ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLoginArgs {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
    /// Required for `provider="anthropic"`. Ignored for OAuth providers,
    /// which drive their own browser/device flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLoginOutput {
    pub provider: String,
    pub stored: bool,
    /// Masked credential identifier (masked key / account login). Mirrors
    /// what `auth.status` would show after the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

pub struct AuthLogin;
impl OrcaToolDef for AuthLogin {
    const NAME: &'static str = "auth.login";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Authenticate with a provider. Anthropic: pass `key`. \
         GitHub: device-flow (prints code + URL, blocks on poll). \
         Atlassian: PKCE callback flow (binds local port, blocks).";
    type Args = AuthLoginArgs;
    type Output = AuthLoginOutput;
}

// ── Native dispatch — all three call into the injected AuthService ──────────

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::auth::AuthService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn AuthService>> {
        ctx.service::<Arc<dyn AuthService>>()
    }

    #[async_trait]
    impl OrcaTool for AuthStatus {
        async fn run(_args: AuthStatusArgs, ctx: &ToolCtx) -> Result<AuthStatusReport> {
            svc(ctx)?.status().await
        }
    }

    #[async_trait]
    impl OrcaTool for AuthLogout {
        async fn run(args: AuthLogoutArgs, ctx: &ToolCtx) -> Result<AuthLogoutOutput> {
            let removed = svc(ctx)?.logout(&args.provider).await?;
            Ok(AuthLogoutOutput {
                provider: args.provider,
                removed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for AuthLogin {
        async fn run(args: AuthLoginArgs, ctx: &ToolCtx) -> Result<AuthLoginOutput> {
            svc(ctx)?.login(&args.provider, args.key.as_deref()).await
        }
    }
}
