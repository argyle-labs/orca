//! orca PKI — flat surface (`pki.{create, list}`). `create` handles both CA
//! initialization and plugin cert issuance via `kind`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;
use std::path::PathBuf;
use utils::pki::{self as sdk_pki, Capability};

fn pki_dir() -> PathBuf {
    // Canonical resolver (honors $ORCA_HOME); was dirs::home_dir() which ignored it.
    contract::config::pki_dir().unwrap_or_default()
}

#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum PkiKind {
    #[default]
    Ca,
    Cert,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCertEntry {
    pub plugin_id: String,
    pub cert_path: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct PkiCreateArgs {
    /// `ca` (default) initializes the orca CA + server cert; `cert` issues a plugin cert.
    #[arg(long, default_value = "ca")]
    pub kind: PkiKind,
    /// (cert) Plugin id to issue for.
    #[arg(long)]
    pub plugin_id: Option<String>,
    /// (cert) `general` (default) or `sensitive`.
    #[arg(long, default_value = "general")]
    pub capability: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PkiCreateOutput {
    // CA fields
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ca_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server_cert_path: String,
    /// Did this call create new material (false when the CA already existed).
    pub created: bool,

    // Cert fields
    #[serde(skip_serializing_if = "String::is_empty")]
    pub plugin_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub capability: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cert_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_path: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PkiListArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiListOutput {
    pub certs: Vec<PkiCertEntry>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// [MUTATES STATE] Initialize orca PKI (CA + server cert) or issue a plugin cert.
/// `kind=ca` is safe to re-run; `kind=cert` requires `plugin_id`.
#[orca_tool(domain = "pki", verb = "create")]
async fn pki_create(
    args: PkiCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<PkiCreateOutput> {
    let dir = pki_dir();
    let mut out = PkiCreateOutput::default();
    match args.kind {
        PkiKind::Ca => {
            let ca_path = sdk_pki::ca_cert_path(&dir);
            let server_cert_path = sdk_pki::server_cert_path(&dir);
            let existed = ca_path.exists();
            sdk_pki::init(&dir)?;
            out.ca_path = ca_path.display().to_string();
            out.server_cert_path = server_cert_path.display().to_string();
            out.created = !existed;
        }
        PkiKind::Cert => {
            let plugin_id = args
                .plugin_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("plugin_id required for kind=cert"))?;
            let cap_str = args.capability.as_deref().unwrap_or("general");
            let cap: Capability = cap_str.parse()?;
            let _bundle = sdk_pki::issue(&dir, plugin_id, cap)?;
            out.plugin_id = plugin_id.to_string();
            out.capability = cap.as_str().into();
            out.cert_path = sdk_pki::plugin_cert_path(&dir, plugin_id)
                .display()
                .to_string();
            out.key_path = sdk_pki::plugin_key_path(&dir, plugin_id)
                .display()
                .to_string();
            out.created = true;
        }
    }
    Ok(out)
}

/// List all issued plugin certs.
#[orca_tool(domain = "pki", verb = "list")]
async fn pki_list(args: PkiListArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<PkiListOutput> {
    let dir = pki_dir();
    let mut ids = sdk_pki::list_plugins(&dir);
    ids.sort();
    let certs: Vec<PkiCertEntry> = ids
        .into_iter()
        .map(|id| PkiCertEntry {
            cert_path: sdk_pki::plugin_cert_path(&dir, &id).display().to_string(),
            plugin_id: id,
        })
        .collect();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(certs, &params);
    Ok(PkiListOutput {
        certs: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}
