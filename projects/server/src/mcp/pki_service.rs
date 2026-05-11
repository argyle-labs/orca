//! Server-side `PkiService` impl — wraps `orca_sdk::pki` helpers.

use anyhow::Result;
use async_trait::async_trait;
use orca_sdk::pki::{self, Capability};
use orca_tools_def::orca_pki::{PkiCertEntry, PkiCertReport, PkiInitReport, PkiListReport};
use orca_tools_def::services::pki::PkiService;
use orca_utils::config::{APP_PKI_DIR, APP_STATE_DIR};
use std::path::PathBuf;

fn pki_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(APP_STATE_DIR)
        .join(APP_PKI_DIR)
}

pub struct ServerPki;

#[async_trait]
impl PkiService for ServerPki {
    async fn ca_init(&self) -> Result<PkiInitReport> {
        let dir = pki_dir();
        let ca_path = pki::ca_cert_path(&dir);
        let server_cert_path = pki::server_cert_path(&dir);
        let existed = ca_path.exists();
        pki::init(&dir)?;
        Ok(PkiInitReport {
            ca_path: ca_path.display().to_string(),
            server_cert_path: server_cert_path.display().to_string(),
            created: !existed,
        })
    }

    async fn cert_issue(&self, plugin_id: &str, capability: &str) -> Result<PkiCertReport> {
        let dir = pki_dir();
        let cap: Capability = capability.parse()?;
        let _bundle = pki::issue(&dir, plugin_id, cap)?;
        Ok(PkiCertReport {
            plugin_id: plugin_id.into(),
            capability: cap.as_str().into(),
            cert_path: pki::plugin_cert_path(&dir, plugin_id).display().to_string(),
            key_path: pki::plugin_key_path(&dir, plugin_id).display().to_string(),
        })
    }

    async fn list(&self) -> Result<PkiListReport> {
        let dir = pki_dir();
        let certs = pki::list_plugins(&dir)
            .into_iter()
            .map(|id| PkiCertEntry {
                cert_path: pki::plugin_cert_path(&dir, &id).display().to_string(),
                plugin_id: id,
            })
            .collect();
        Ok(PkiListReport { certs })
    }
}
