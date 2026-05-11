//! `PkiService` — orca CA + plugin certificate management.

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_pki::{PkiCertReport, PkiInitReport, PkiListReport};

#[async_trait]
pub trait PkiService: Send + Sync {
    async fn ca_init(&self) -> Result<PkiInitReport>;
    async fn cert_issue(&self, plugin_id: &str, capability: &str) -> Result<PkiCertReport>;
    async fn list(&self) -> Result<PkiListReport>;
}
