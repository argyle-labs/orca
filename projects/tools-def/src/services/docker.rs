//! Service trait for the `docker` domain — engine state, compose-project
//! service listing + lifecycle actions, and per-project log fetch (incl. the
//! cross-project `list_log_services` aggregator).
//!
//! Logs live alongside docker because the orca log endpoints read container
//! logs through `docker compose logs`. If/when logs start sourcing from
//! elsewhere (journald, files), they can split into their own service.

use anyhow::Result;
use async_trait::async_trait;

use crate::docker::{DockerActionResult, DockerEngineStatus, DockerLogProject, DockerServicesView};

#[async_trait]
pub trait DockerService: Send + Sync {
    /// Probe the local docker engine — colima / desktop / none + running flag.
    async fn engine_status(&self) -> Result<DockerEngineStatus>;

    /// Start the local docker engine. Returns the start-command output.
    async fn engine_start(&self) -> Result<String>;

    /// Resolve the compose file under `project_path` and list its services
    /// with state/health/ports.
    async fn services(&self, project_path: &str) -> Result<DockerServicesView>;

    /// Run a compose lifecycle action (`up`, `down`, `restart`, `logs`, ...).
    async fn action(
        &self,
        project_path: &str,
        action: &str,
        service: Option<&str>,
        tail: Option<u32>,
    ) -> Result<DockerActionResult>;

    /// Fetch container logs for a single compose project. `service = None`
    /// reads logs across every service in that project.
    async fn logs(&self, project_path: &str, service: Option<&str>, tail: u32) -> Result<String>;

    /// Walk the rebuy root and return every compose project + its service
    /// state. Used by the cross-project logs panel.
    async fn log_services(&self) -> Result<Vec<DockerLogProject>>;
}
