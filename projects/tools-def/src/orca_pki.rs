//! orca PKI — CA initialization, plugin cert issuance, listing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

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

#[cfg(feature = "native")]
fn pki_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::pki::PkiService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::pki::PkiService>>()
}

/// [MUTATES STATE] Initialize the orca CA and server cert. Safe to re-run; skips if CA exists.
#[orca_tool(domain = "pki", verb = "ca-init")]
async fn pki_ca_init(
    _args: PkiCaInitArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PkiInitReport> {
    pki_svc(ctx)?.ca_init().await
}

/// [MUTATES STATE] Issue a cert for a plugin.
#[orca_tool(domain = "pki", verb = "cert-issue")]
async fn pki_cert_issue(
    args: PkiCertIssueArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PkiCertReport> {
    pki_svc(ctx)?
        .cert_issue(&args.plugin_id, &args.capability)
        .await
}

/// List all issued plugin certs.
#[orca_tool(domain = "pki", verb = "list")]
async fn pki_list(
    _args: PkiListArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<PkiListReport> {
    pki_svc(ctx)?.list().await
}
