//! Server-side `AuthService` impl — wraps existing `commands::{auth,oauth}`
//! helpers behind the unified `auth.{status,logout,login}` surface.

use anyhow::{Result, bail};
use async_trait::async_trait;
use orca_tools_def::orca_auth::{
    ApiTokenSummary, AuthLoginOutput, AuthProviderStatus, AuthStatusReport, TokenCreateOutput,
};
use orca_tools_def::services::auth::AuthService;
use rand::Rng;
use sha2::{Digest, Sha256};

const ANTHROPIC_KEY: &str = "anthropic_api_key";

pub struct ServerAuth;

#[async_trait]
impl AuthService for ServerAuth {
    async fn status(&self) -> Result<AuthStatusReport> {
        let conn = db::open_default()?;
        let anthropic = db::settings::secret_get(&conn, ANTHROPIC_KEY)?;
        let github = crate::commands::oauth::load_github_token();
        let atlassian = crate::commands::oauth::load_atlassian_access_token();
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

    async fn logout(&self, provider: &str) -> Result<bool> {
        match provider {
            "anthropic" => {
                let conn = db::open_default()?;
                Ok(db::settings::secret_delete(&conn, ANTHROPIC_KEY)?)
            }
            "github" => Ok(crate::commands::oauth::delete_oauth_silent("github")),
            "atlassian" => Ok(crate::commands::oauth::delete_oauth_silent("atlassian")),
            other => bail!("unknown provider '{other}' (want: anthropic|github|atlassian)"),
        }
    }

    async fn login(&self, provider: &str, key: Option<&str>) -> Result<AuthLoginOutput> {
        match provider {
            "anthropic" => {
                let key = key
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
                crate::commands::oauth::cmd_oauth_github().await?;
                let id = crate::commands::oauth::load_github_token()
                    .as_deref()
                    .map(db::settings::mask_key);
                Ok(AuthLoginOutput {
                    provider: provider.into(),
                    stored: id.is_some(),
                    identity: id,
                })
            }
            "atlassian" => {
                crate::commands::oauth::cmd_oauth_atlassian().await?;
                let id = crate::commands::oauth::load_atlassian_access_token()
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

    async fn token_create(
        &self,
        name: &str,
        role: &str,
        expires_in_days: Option<u32>,
    ) -> Result<TokenCreateOutput> {
        if !matches!(role, "admin" | "read") {
            bail!("role must be 'admin' or 'read', got '{role}'");
        }
        // 16 random bytes → 32 hex chars. `orca_` prefix keeps tokens
        // self-identifying in logs/secret-scanners.
        let mut raw = [0u8; 16];
        rand::rng().fill_bytes(&mut raw);
        let plaintext = format!("orca_{}", hex_lower(&raw));
        let token_hash = sha256_hex(plaintext.as_bytes());

        let id = uuid::Uuid::now_v7().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let expires_at = expires_in_days
            .map(|d| (chrono::Utc::now() + chrono::Duration::days(d as i64)).to_rfc3339());

        let conn = db::open_default()?;
        db::api_tokens::insert(
            &conn,
            &id,
            name,
            &token_hash,
            role,
            &now,
            expires_at.as_deref(),
        )?;
        Ok(TokenCreateOutput {
            id,
            name: name.to_string(),
            token: plaintext,
        })
    }

    async fn token_list(&self) -> Result<Vec<ApiTokenSummary>> {
        let conn = db::open_default()?;
        let rows = db::api_tokens::list(&conn)?;
        Ok(rows
            .into_iter()
            .map(|r| ApiTokenSummary {
                id: r.id,
                name: r.name,
                role: r.role,
                created_at: r.created_at,
                last_used_at: r.last_used_at,
                expires_at: r.expires_at,
            })
            .collect())
    }

    async fn token_revoke(&self, id: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::api_tokens::revoke(&conn, id)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256_hex(input: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(input);
    hex_lower(&h.finalize())
}
