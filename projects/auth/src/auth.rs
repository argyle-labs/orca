//! Auth domain — unified surface for credential management across providers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use anyhow::bail;
use derive::orca_tool;
use rand::Rng;
use utils::hash;

const ANTHROPIC_KEY: &str = "anthropic_api_key";

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
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AuthStatusReport> {
    let conn = db::open_default()?;
    let anthropic = db::settings::secret_get(&conn, ANTHROPIC_KEY)?;
    let github = crate::oauth::load_github_token();
    let atlassian = crate::oauth::load_atlassian_access_token();
    Ok(AuthStatusReport {
        providers: vec![
            AuthProviderStatus {
                provider: "anthropic".into(),
                configured: anthropic.is_some(),
                identity: anthropic.as_deref().map(db::settings::mask_key),
            },
            AuthProviderStatus {
                provider: "github".into(),
                configured: github.is_some(),
                identity: github.as_deref().map(db::settings::mask_key),
            },
            AuthProviderStatus {
                provider: "atlassian".into(),
                configured: atlassian.is_some(),
                identity: atlassian.as_deref().map(db::settings::mask_key),
            },
        ],
    })
}

/// [MUTATES STATE] Remove a stored credential. `removed=false` if nothing was stored.
#[orca_tool(domain = "system.auth.session", verb = "delete")]
async fn auth_session_delete(
    args: AuthLogoutArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AuthLogoutOutput> {
    let removed = match args.provider.as_str() {
        "anthropic" => {
            let conn = db::open_default()?;
            db::settings::secret_delete(&conn, ANTHROPIC_KEY)?
        }
        "github" => crate::oauth::delete_oauth_silent("github"),
        "atlassian" => crate::oauth::delete_oauth_silent("atlassian"),
        other => bail!("unknown provider '{other}' (want: anthropic|github|atlassian)"),
    };
    Ok(AuthLogoutOutput {
        provider: args.provider,
        removed,
    })
}

/// [MUTATES STATE] Authenticate with a provider. Anthropic: pass `key`. GitHub: device-flow. Atlassian: PKCE.
#[orca_tool(domain = "system.auth.session", verb = "create")]
async fn auth_session_create(
    args: AuthLoginArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AuthLoginOutput> {
    let provider = args.provider.as_str();
    match provider {
        "anthropic" => {
            let key = args
                .key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("`key` is required when provider=anthropic"))?;
            let conn = db::open_default()?;
            db::settings::secret_set(&conn, ANTHROPIC_KEY, key)?;
            Ok(AuthLoginOutput {
                provider: provider.into(),
                stored: true,
                identity: Some(db::settings::mask_key(key)),
            })
        }
        "github" => {
            crate::oauth::cmd_oauth_github().await?;
            let id = crate::oauth::load_github_token()
                .as_deref()
                .map(db::settings::mask_key);
            Ok(AuthLoginOutput {
                provider: provider.into(),
                stored: id.is_some(),
                identity: id,
            })
        }
        "atlassian" => {
            crate::oauth::cmd_oauth_atlassian().await?;
            let id = crate::oauth::load_atlassian_access_token()
                .as_deref()
                .map(db::settings::mask_key);
            Ok(AuthLoginOutput {
                provider: provider.into(),
                stored: id.is_some(),
                identity: id,
            })
        }
        other => bail!("unknown provider '{other}' (want: anthropic|github|atlassian)"),
    }
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
    ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenCreateOutput> {
    if !matches!(args.role.as_str(), "admin" | "read") {
        bail!("role must be 'admin' or 'read', got '{}'", args.role);
    }
    // 16 random bytes → 32 hex chars. `orca_` prefix keeps tokens
    // self-identifying in logs/secret-scanners.
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let plaintext = format!("orca_{}", hash::hex_encode(&raw));
    let token_hash = hash::sha256_hex(plaintext.as_bytes());

    let id = uuid::Uuid::now_v7().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let expires_at = args
        .expires_in_days
        .map(|d| (chrono::Utc::now() + chrono::Duration::days(d as i64)).to_rfc3339());

    // Bind the new token to the authenticated operator so later bearer-auth
    // requests resolve to a real user (S4 of [[project-remote-exec-full-fix]]).
    // The bootstrap path (first token, no auth yet) has no caller → user_id
    // is NULL and that token authenticates only locally.
    let caller_user_id = ctx.caller().map(|c| c.user_id);
    let conn = db::open_default()?;
    db::api_tokens::insert(
        &conn,
        &id,
        &args.name,
        &token_hash,
        &args.role,
        &now,
        expires_at.as_deref(),
        caller_user_id.as_deref(),
    )?;
    Ok(TokenCreateOutput {
        id,
        name: args.name,
        token: plaintext,
    })
}

/// List all REST/MCP bearer tokens registered on this host. Token hashes are not returned.
#[orca_tool(domain = "system.auth.token", verb = "list")]
async fn auth_token_list(
    _args: TokenListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenListOutput> {
    let conn = db::open_default()?;
    let rows = db::api_tokens::list(&conn)?;
    let tokens = rows
        .into_iter()
        .map(|r| ApiTokenSummary {
            id: r.id,
            name: r.name,
            role: r.role,
            created_at: r.created_at,
            last_used_at: r.last_used_at,
            expires_at: r.expires_at,
        })
        .collect();
    Ok(TokenListOutput { tokens })
}

/// [MUTATES STATE] Revoke a token by id. Returns `revoked=false` if the id wasn't found.
#[orca_tool(domain = "system.auth.token", verb = "delete")]
async fn auth_token_delete(
    args: TokenRevokeArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<TokenRevokeOutput> {
    let conn = db::open_default()?;
    let revoked = db::api_tokens::revoke(&conn, &args.id)?;
    Ok(TokenRevokeOutput { revoked })
}

// Hex / sha helpers used to live here; replaced by utils::hash::*.
