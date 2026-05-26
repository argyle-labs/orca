//! Auth domain — unified surface for credential management across providers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

// ── Shared rows ─────────────────────────────────────────────────────────────

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

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct AuthStatusReport {
    pub providers: Vec<AuthProviderStatus>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthStatusArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLogoutArgs {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLogoutOutput {
    pub provider: String,
    pub removed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLoginArgs {
    /// "anthropic" | "github" | "atlassian"
    pub provider: String,
    /// Required for `provider="anthropic"`. Ignored for OAuth providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AuthLoginOutput {
    pub provider: String,
    pub stored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// Snapshot every configured credential the host knows about (Anthropic key + OAuth tokens).
#[orca_tool(domain = "system.auth.session", verb = "detail")]
async fn auth_session_detail(
    _args: AuthStatusArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<AuthStatusReport> {
    ctx.service::<Arc<dyn AuthService>>()?.status().await
}

/// [MUTATES STATE] Remove a stored credential. `removed=false` if nothing was stored.
#[orca_tool(domain = "system.auth.session", verb = "delete")]
async fn auth_session_delete(
    args: AuthLogoutArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<AuthLogoutOutput> {
    let removed = ctx
        .service::<Arc<dyn AuthService>>()?
        .logout(&args.provider)
        .await?;
    Ok(AuthLogoutOutput {
        provider: args.provider,
        removed,
    })
}

/// [MUTATES STATE] Authenticate with a provider. Anthropic: pass `key`. GitHub: device-flow. Atlassian: PKCE.
#[orca_tool(domain = "system.auth.session", verb = "create")]
async fn auth_session_create(
    args: AuthLoginArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<AuthLoginOutput> {
    ctx.service::<Arc<dyn AuthService>>()?
        .login(&args.provider, args.key.as_deref())
        .await
}

// ── API tokens (REST/MCP bearer auth, local-host scope) ─────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ApiTokenSummary {
    pub id: String,
    pub name: String,
    /// "admin" | "read"
    pub role: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenCreateArgs {
    /// Human-readable label (e.g. "ci-runner", "scott-laptop"). Must be unique on this host.
    pub name: String,
    /// "admin" | "read"
    pub role: String,
    /// Days until expiry. `None` = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenCreateOutput {
    pub id: String,
    pub name: String,
    /// Plaintext bearer token — returned exactly once. Store it now; it is
    /// unrecoverable from the DB.
    pub token: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenListOutput {
    pub tokens: Vec<ApiTokenSummary>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenRevokeArgs {
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenRevokeOutput {
    pub revoked: bool,
}

/// [MUTATES STATE] Mint a new REST/MCP bearer token on THIS host. Plaintext is
/// returned exactly once and cannot be recovered from the DB. Token only
/// authenticates calls to this host's `:12000` — not to other peers.
#[orca_tool(domain = "system.auth.token", verb = "create")]
async fn auth_token_create(
    args: TokenCreateArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<TokenCreateOutput> {
    ctx.service::<Arc<dyn AuthService>>()?
        .token_create(&args.name, &args.role, args.expires_in_days)
        .await
}

/// List all REST/MCP bearer tokens registered on this host. Token hashes are not returned.
#[orca_tool(domain = "system.auth.token", verb = "list")]
async fn auth_token_list(
    _args: TokenListArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<TokenListOutput> {
    let tokens = ctx.service::<Arc<dyn AuthService>>()?.token_list().await?;
    Ok(TokenListOutput { tokens })
}

/// [MUTATES STATE] Revoke a token by id. Returns `revoked=false` if the id wasn't found.
#[orca_tool(domain = "system.auth.token", verb = "delete")]
async fn auth_token_delete(
    args: TokenRevokeArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<TokenRevokeOutput> {
    let revoked = ctx
        .service::<Arc<dyn AuthService>>()?
        .token_revoke(&args.id)
        .await?;
    Ok(TokenRevokeOutput { revoked })
}

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait AuthService: Send + Sync {
    /// Snapshot every configured/unconfigured credential the host knows about.
    async fn status(&self) -> Result<AuthStatusReport>;

    /// Remove a stored credential. Returns `true` if anything was removed.
    /// `provider` ∈ { "anthropic", "github", "atlassian" }.
    async fn logout(&self, provider: &str) -> Result<bool>;

    /// Authenticate with `provider`. For `anthropic` the caller must supply
    /// `key`. For OAuth providers (`github`, `atlassian`) `key` is ignored
    /// and the method drives the device-flow or PKCE callback to completion
    /// before returning.
    async fn login(&self, provider: &str, key: Option<&str>) -> Result<AuthLoginOutput>;

    /// Mint a new API bearer token in THIS host's `api_tokens` table. The
    /// plaintext is returned exactly once and never recoverable. Tokens are
    /// scoped to the local REST API only — they don't authenticate calls to
    /// other peers in the pod.
    async fn token_create(
        &self,
        name: &str,
        role: &str,
        expires_in_days: Option<u32>,
    ) -> Result<TokenCreateOutput>;

    /// List all tokens registered on this host (hash excluded).
    async fn token_list(&self) -> Result<Vec<ApiTokenSummary>>;

    /// Revoke a token by id. Returns true if a row was deleted.
    async fn token_revoke(&self, id: &str) -> Result<bool>;
}

/// Embedder hook — see `services::mod` doc.
pub trait ProvideAuth {
    fn auth(&self) -> std::sync::Arc<dyn AuthService>;
}

/// Register a `AuthService` into `ToolCtx`.
pub fn register_auth(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideAuth) {
    ctx.register_service(p.auth());
}
