//! orca PKI — CA initialization, plugin cert issuance, listing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiInitReport {
    pub ca_path: String,
    pub server_cert_path: String,
    pub created: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCertReport {
    pub plugin_id: String,
    pub capability: String,
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCertEntry {
    pub plugin_id: String,
    pub cert_path: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiListReport {
    pub certs: Vec<PkiCertEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCaInitArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCertIssueArgs {
    pub plugin_id: String,
    /// "general" (default) or "sensitive".
    #[serde(default = "default_capability")]
    #[cfg_attr(feature = "cli", arg(default_value = "general"))]
    pub capability: String,
}
fn default_capability() -> String {
    "general".into()
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiListArgs {}

/// [MUTATES STATE] Initialize the orca CA and server cert. Safe to re-run; skips if CA exists.
#[orca_tool(domain = "system.pki.ca", verb = "create")]
async fn pki_ca_create(
    _args: PkiCaInitArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PkiInitReport> {
    ctx.service::<Arc<dyn PkiService>>()?.ca_init().await
}

/// [MUTATES STATE] Issue a cert for a plugin.
#[orca_tool(domain = "system.pki.cert", verb = "create")]
async fn pki_cert_create(
    args: PkiCertIssueArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PkiCertReport> {
    ctx.service::<Arc<dyn PkiService>>()?
        .cert_issue(&args.plugin_id, &args.capability)
        .await
}

/// List all issued plugin certs.
#[orca_tool(domain = "system.pki", verb = "list")]
async fn pki_list(
    _args: PkiListArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PkiListReport> {
    ctx.service::<Arc<dyn PkiService>>()?.list().await
}

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait PkiService: Send + Sync {
    async fn ca_init(&self) -> Result<PkiInitReport>;
    async fn cert_issue(&self, plugin_id: &str, capability: &str) -> Result<PkiCertReport>;
    async fn list(&self) -> Result<PkiListReport>;
}

/// Embedder hook — see `services::mod` doc.
pub trait ProvidePki {
    fn pki(&self) -> std::sync::Arc<dyn PkiService>;
}

/// Register a `PkiService` into `ToolCtx`.
pub fn register_pki(ctx: &mut orca_contract::ToolCtx, p: &impl ProvidePki) {
    ctx.register_service(p.pki());
}
