//! Infra domain tools — docker compose service listing, log fetch, test runner.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ServiceState {
    pub name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    pub ports: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectServices {
    pub project: String,
    pub path: String,
    pub services: Vec<ServiceState>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListServicesArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListServicesOutput {
    pub projects: Vec<ProjectServices>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetServiceLogsArgs {
    /// Absolute path to the project directory.
    pub project: String,
    /// Service name as defined in docker-compose.
    pub service: String,
    /// Number of log lines to return (default: 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u64>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetServiceLogsOutput {
    pub project: String,
    pub service: String,
    pub output: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunTestsArgs {
    /// Which suite to run: rust | frontend | e2e | all (default: rust).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunTestsOutput {
    pub suite: String,
    pub output: String,
    pub exit_code: i32,
    pub passed: u32,
    pub failed: u32,
    pub duration_ms: u64,
}

#[cfg(feature = "native")]
fn infra_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::infra::InfraService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::infra::InfraService>>()
}

/// List all running docker compose services across all rebuy projects. Returns
/// project name, path, and per-service state/health/ports.
#[orca_tool(domain = "infra", verb = "services")]
async fn list_services(
    _args: ListServicesArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListServicesOutput> {
    let projects = infra_svc(ctx)?
        .list_services()
        .await?
        .into_iter()
        .map(|p| ProjectServices {
            project: p.project,
            path: p.path,
            services: p
                .services
                .into_iter()
                .map(|s| ServiceState {
                    name: s.name,
                    state: s.state,
                    health: s.health,
                    ports: s.ports,
                })
                .collect(),
        })
        .collect();
    Ok(ListServicesOutput { projects })
}

/// Fetch docker compose logs for a running rebuy service. Specify the project
/// path and service name.
#[orca_tool(domain = "infra", verb = "service-logs")]
async fn get_service_logs(
    args: GetServiceLogsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetServiceLogsOutput> {
    let tail = args.tail.unwrap_or(200);
    let output = infra_svc(ctx)?
        .service_logs(&args.project, &args.service, tail)
        .await?;
    Ok(GetServiceLogsOutput {
        project: args.project,
        service: args.service,
        output,
    })
}

/// Run the orca project test suite. Returns test output with pass/fail counts.
/// Suites: rust (cargo test), frontend (vitest), e2e (playwright), all.
#[orca_tool(domain = "infra", verb = "run-tests")]
async fn run_tests(
    args: RunTestsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<RunTestsOutput> {
    let suite = args.suite.as_deref().unwrap_or("rust");
    let r = infra_svc(ctx)?.run_tests(suite).await?;
    Ok(RunTestsOutput {
        suite: r.suite,
        output: r.output,
        exit_code: r.exit_code,
        passed: r.passed,
        failed: r.failed,
        duration_ms: r.duration_ms,
    })
}
