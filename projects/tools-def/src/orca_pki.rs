//! orca PKI — CA initialization, plugin cert issuance, listing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiInitReport {
    pub ca_path: String,
    pub server_cert_path: String,
    pub created: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCertReport {
    pub plugin_id: String,
    pub capability: String,
    pub cert_path: String,
    pub key_path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCertEntry {
    pub plugin_id: String,
    pub cert_path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiListReport {
    pub certs: Vec<PkiCertEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiCaInitArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PkiListArgs {}

pub struct PkiCaInit;
impl OrcaToolDef for PkiCaInit {
    const NAME: &'static str = "pki.ca-init";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Initialize the orca CA and server cert. Safe to re-run; skips if CA exists.";
    type Args = PkiCaInitArgs;
    type Output = PkiInitReport;
}

pub struct PkiCertIssue;
impl OrcaToolDef for PkiCertIssue {
    const NAME: &'static str = "pki.cert-issue";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Issue a cert for a plugin.";
    type Args = PkiCertIssueArgs;
    type Output = PkiCertReport;
}

pub struct PkiList;
impl OrcaToolDef for PkiList {
    const NAME: &'static str = "pki.list";
    const DESCRIPTION: &'static str = "List all issued plugin certs.";
    type Args = PkiListArgs;
    type Output = PkiListReport;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::pki::PkiService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn PkiService>> {
        ctx.service::<Arc<dyn PkiService>>()
    }

    #[async_trait]
    impl OrcaTool for PkiCaInit {
        async fn run(_a: PkiCaInitArgs, ctx: &ToolCtx) -> Result<PkiInitReport> {
            svc(ctx)?.ca_init().await
        }
    }
    #[async_trait]
    impl OrcaTool for PkiCertIssue {
        async fn run(a: PkiCertIssueArgs, ctx: &ToolCtx) -> Result<PkiCertReport> {
            svc(ctx)?.cert_issue(&a.plugin_id, &a.capability).await
        }
    }
    #[async_trait]
    impl OrcaTool for PkiList {
        async fn run(_a: PkiListArgs, ctx: &ToolCtx) -> Result<PkiListReport> {
            svc(ctx)?.list().await
        }
    }
}
