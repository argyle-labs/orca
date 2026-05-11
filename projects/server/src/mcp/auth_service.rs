//! Server-side `AuthService` impl — wraps existing `commands::{auth,oauth}`
//! helpers behind the unified `auth.{status,logout,login}` surface.

use anyhow::{Result, bail};
use async_trait::async_trait;
use orca_tools_def::orca_auth::{AuthLoginOutput, AuthProviderStatus, AuthStatusReport};
use orca_tools_def::services::auth::AuthService;

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
}
