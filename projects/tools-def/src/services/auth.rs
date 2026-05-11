//! `AuthService` — abstracts credential storage and OAuth flows so the auth
//! tools (login / logout / status) can live in this wasm-safe crate while
//! the actual server-side device-flow and PKCE plumbing stays in
//! `projects/server/src/commands/{auth,oauth}.rs`.
//!
//! All three methods are async and may take seconds (device-flow polling) —
//! callers should be prepared for that latency.

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_auth::{AuthLoginOutput, AuthStatusReport};

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
}
