//! Infra domain tools — docker compose service listing, log fetch, test runner.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ServiceState {
    pub name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    pub ports: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectServices {
    pub project: String,
    pub path: String,
    pub services: Vec<ServiceState>,
}

// ── list_services ───────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListServicesArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListServicesOutput {
    pub projects: Vec<ProjectServices>,
}

pub struct ListServices;
impl OrcaToolDef for ListServices {
    const NAME: &'static str = "list_services";
    const DESCRIPTION: &'static str = "List all running docker compose services across all rebuy \
         projects. Returns project name, path, and per-service state/health/ports.";
    type Args = ListServicesArgs;
    type Output = ListServicesOutput;
}

// ── get_service_logs ────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetServiceLogsOutput {
    pub project: String,
    pub service: String,
    pub output: String,
}

pub struct GetServiceLogs;
impl OrcaToolDef for GetServiceLogs {
    const NAME: &'static str = "get_service_logs";
    const DESCRIPTION: &'static str = "Fetch docker compose logs for a running rebuy service. \
         Specify the project path and service name.";
    type Args = GetServiceLogsArgs;
    type Output = GetServiceLogsOutput;
}

// ── run_tests ───────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunTestsArgs {
    /// Which suite to run: rust | frontend | e2e | all (default: rust).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

pub struct RunTests;
impl OrcaToolDef for RunTests {
    const NAME: &'static str = "run_tests";
    const DESCRIPTION: &'static str = "Run the orca project test suite. Returns test output with \
         pass/fail counts. Suites: rust (cargo test), frontend (vitest), e2e (playwright), all.";
    type Args = RunTestsArgs;
    type Output = RunTestsOutput;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::infra as svc_infra;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn svc_infra::InfraService>> {
        ctx.service::<Arc<dyn svc_infra::InfraService>>()
    }

    #[async_trait]
    impl OrcaTool for ListServices {
        async fn run(_args: ListServicesArgs, ctx: &ToolCtx) -> Result<ListServicesOutput> {
            let projects = svc(ctx)?
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
    }

    #[async_trait]
    impl OrcaTool for GetServiceLogs {
        async fn run(args: GetServiceLogsArgs, ctx: &ToolCtx) -> Result<GetServiceLogsOutput> {
            let tail = args.tail.unwrap_or(200);
            let output = svc(ctx)?
                .service_logs(&args.project, &args.service, tail)
                .await?;
            Ok(GetServiceLogsOutput {
                project: args.project,
                service: args.service,
                output,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for RunTests {
        async fn run(args: RunTestsArgs, ctx: &ToolCtx) -> Result<RunTestsOutput> {
            let suite = args.suite.as_deref().unwrap_or("rust");
            let r = svc(ctx)?.run_tests(suite).await?;
            Ok(RunTestsOutput {
                suite: r.suite,
                output: r.output,
                exit_code: r.exit_code,
                passed: r.passed,
                failed: r.failed,
                duration_ms: r.duration_ms,
            })
        }
    }
}
