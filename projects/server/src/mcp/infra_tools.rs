use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::mcp::handlers;

// ── list_services ─────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListServicesArgs {}

pub struct ListServices;

#[async_trait]
impl OrcaTool for ListServices {
    const NAME: &'static str = "list_services";
    const DESCRIPTION: &'static str = "List all running docker compose services across all rebuy projects. \
         Returns project name, path, and per-service state/health/ports.";
    type Args = ListServicesArgs;
    async fn run(_: ListServicesArgs, _: &ToolCtx) -> Result<String> {
        handlers::list_services().await
    }
}

// ── get_service_logs ──────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetServiceLogsArgs {
    /// Absolute path to the project directory
    pub project: String,
    /// Service name as defined in docker-compose
    pub service: String,
    /// Number of log lines to return (default: 200)
    pub tail: Option<u64>,
}

pub struct GetServiceLogs;

#[async_trait]
impl OrcaTool for GetServiceLogs {
    const NAME: &'static str = "get_service_logs";
    const DESCRIPTION: &'static str = "Fetch docker compose logs for a running rebuy service. \
         Specify the project path and service name.";
    type Args = GetServiceLogsArgs;
    async fn run(args: GetServiceLogsArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::service_logs(&json!({
            "project": args.project,
            "service": args.service,
            "tail": args.tail
        }))
        .await
    }
}

// ── run_tests ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct RunTestsArgs {
    /// Which suite to run: rust | frontend | e2e | all (default: rust)
    pub suite: Option<String>,
}

pub struct RunTests;

#[async_trait]
impl OrcaTool for RunTests {
    const NAME: &'static str = "run_tests";
    const DESCRIPTION: &'static str = "Run the orca project test suite. Returns test output with pass/fail counts. \
         Suites: rust (cargo test), frontend (vitest), e2e (playwright), all.";
    type Args = RunTestsArgs;
    async fn run(args: RunTestsArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::run_tests(&json!({ "suite": args.suite })).await
    }
}

// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<ListServices>()
        .register::<GetServiceLogs>()
        .register::<RunTests>();
}
