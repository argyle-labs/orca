//! Common *arr operations as plain reusable async fns.
//!
//! The per-app plugins (`sonarr`, `radarr`, `prowlarr`, `lidarr`) are thin
//! `#[orca_tool]` wrappers that resolve their endpoint row, build an
//! [`crate::Client`] with the app's fixed [`crate::Flavor`], and call one of
//! these. Keeping the HTTP logic here keeps every per-app plugin DRY and means
//! a fix lands once for the whole stack.

use crate::{
    ArrError, Client, HealthIssue, Indexer, IndexerStatus, IndexerTestResult, SystemStatus,
};

/// **Detection.** Health issues currently raised by the server.
pub async fn health(client: &Client) -> Result<Vec<HealthIssue>, ArrError> {
    client.health().await
}

/// Configured indexers.
pub async fn indexers(client: &Client) -> Result<Vec<Indexer>, ArrError> {
    client.indexers().await
}

/// Prowlarr per-indexer backoff status (only meaningful on prowlarr).
pub async fn indexer_status(client: &Client) -> Result<Vec<IndexerStatus>, ArrError> {
    client.indexer_status().await
}

/// **Remediation.** Re-test every indexer — clears stale backoff.
pub async fn test_indexers(client: &Client) -> Result<Vec<IndexerTestResult>, ArrError> {
    client.test_indexers().await
}

/// App name / version / instance name.
pub async fn system_status(client: &Client) -> Result<SystemStatus, ArrError> {
    client.system_status().await
}
