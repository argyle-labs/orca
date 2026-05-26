//! orca PKI — CA initialization, plugin cert issuance, listing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;
#[cfg(feature = "native")]
use orca_sdk::pki::{self as sdk_pki, Capability};
#[cfg(feature = "native")]
use orca_utils::config::{APP_PKI_DIR, APP_STATE_DIR};
#[cfg(feature = "native")]
use std::path::PathBuf;

#[cfg(feature = "native")]
fn pki_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(APP_STATE_DIR)
        .join(APP_PKI_DIR)
}

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
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PkiInitReport> {
    let dir = pki_dir();
    let ca_path = sdk_pki::ca_cert_path(&dir);
    let server_cert_path = sdk_pki::server_cert_path(&dir);
    let existed = ca_path.exists();
    sdk_pki::init(&dir)?;
    Ok(PkiInitReport {
        ca_path: ca_path.display().to_string(),
        server_cert_path: server_cert_path.display().to_string(),
        created: !existed,
    })
}

/// [MUTATES STATE] Issue a cert for a plugin.
#[orca_tool(domain = "system.pki.cert", verb = "create")]
async fn pki_cert_create(
    args: PkiCertIssueArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PkiCertReport> {
    let dir = pki_dir();
    let cap: Capability = args.capability.parse()?;
    let _bundle = sdk_pki::issue(&dir, &args.plugin_id, cap)?;
    Ok(PkiCertReport {
        plugin_id: args.plugin_id.clone(),
        capability: cap.as_str().into(),
        cert_path: sdk_pki::plugin_cert_path(&dir, &args.plugin_id)
            .display()
            .to_string(),
        key_path: sdk_pki::plugin_key_path(&dir, &args.plugin_id)
            .display()
            .to_string(),
    })
}

/// List all issued plugin certs.
#[orca_tool(domain = "system.pki", verb = "list")]
async fn pki_list(
    _args: PkiListArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<PkiListReport> {
    let dir = pki_dir();
    let certs = sdk_pki::list_plugins(&dir)
        .into_iter()
        .map(|id| PkiCertEntry {
            cert_path: sdk_pki::plugin_cert_path(&dir, &id).display().to_string(),
            plugin_id: id,
        })
        .collect();
    Ok(PkiListReport { certs })
}
