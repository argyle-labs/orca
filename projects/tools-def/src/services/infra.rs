//! Service trait for the `infra` domain — docker compose service listing,
//! per-service log fetch, test-suite runner.

use anyhow::Result;
use async_trait::async_trait;

#[derive(Clone)]
pub struct InfraServiceState {
    pub name: String,
    pub state: String,
    pub health: Option<String>,
    pub ports: Vec<String>,
}

#[derive(Clone)]
pub struct InfraProject {
    pub project: String,
    pub path: String,
    pub services: Vec<InfraServiceState>,
}

#[derive(Clone)]
pub struct TestRunResult {
    pub suite: String,
    pub output: String,
    pub exit_code: i32,
    pub passed: u32,
    pub failed: u32,
    pub duration_ms: u64,
}

#[async_trait]
pub trait InfraService: Send + Sync {
    async fn list_services(&self) -> Result<Vec<InfraProject>>;
    async fn service_logs(&self, project: &str, service: &str, tail: u64) -> Result<String>;
    async fn run_tests(&self, suite: &str) -> Result<TestRunResult>;
}
