//! Docker domain tools — engine state, compose service listing, lifecycle
//! actions, log fetch, and the cross-project log services aggregator.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Shared row shapes ───────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "lowercase")]
pub enum DockerEngineKind {
    Colima,
    Desktop,
    None,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerEngineStatus {
    pub engine: DockerEngineKind,
    pub running: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerServiceRow {
    pub name: String,
    pub state: String,
    pub running: bool,
    pub health: String,
    pub ports: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DockerServicesView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    pub services: Vec<DockerServiceRow>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DockerActionResult {
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerLogProject {
    pub project: String,
    pub path: String,
    pub services: Vec<DockerServiceRow>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Tool args/outputs
// ═══════════════════════════════════════════════════════════════════════════

// get_docker_engine
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetDockerEngineArgs {}

pub struct GetDockerEngine;
impl OrcaToolDef for GetDockerEngine {
    const NAME: &'static str = "get_docker_engine";
    const DESCRIPTION: &'static str =
        "Probe the local docker engine (colima | desktop | none) and whether it is running.";
    type Args = GetDockerEngineArgs;
    type Output = DockerEngineStatus;
}

// start_docker_engine
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StartDockerEngineArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StartDockerEngineOutput {
    pub output: String,
}

pub struct StartDockerEngine;
impl OrcaToolDef for StartDockerEngine {
    const NAME: &'static str = "start_docker_engine";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Start the local docker engine. Returns the start-command output.";
    type Args = StartDockerEngineArgs;
    type Output = StartDockerEngineOutput;
}

// get_docker_services
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetDockerServicesArgs {
    /// Absolute path to the docker-compose project directory.
    pub path: String,
}

pub struct GetDockerServices;
impl OrcaToolDef for GetDockerServices {
    const NAME: &'static str = "get_docker_services";
    const DESCRIPTION: &'static str = "List the compose services under `path` with state/health/ports plus the resolved \
         compose-file path.";
    type Args = GetDockerServicesArgs;
    type Output = DockerServicesView;
}

// run_docker_action
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunDockerActionArgs {
    pub project_path: String,
    /// `up`, `down`, `restart`, `start`, `stop`, `build`, `pull`, `logs`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u32>,
}

pub struct RunDockerAction;
impl OrcaToolDef for RunDockerAction {
    const NAME: &'static str = "run_docker_action";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Run a docker-compose lifecycle action against the compose project at \
         `project_path`.";
    type Args = RunDockerActionArgs;
    type Output = DockerActionResult;
}

// get_logs
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogsArgs {
    /// Absolute path to the compose project.
    pub project: String,
    /// Specific service name; omit to read across all services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Number of log lines to return (default 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u32>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogsOutput {
    pub output: String,
}

pub struct GetLogs;
impl OrcaToolDef for GetLogs {
    const NAME: &'static str = "get_logs";
    const DESCRIPTION: &'static str =
        "Read docker-compose logs from the project at `project` (optionally scoped to a service).";
    type Args = GetLogsArgs;
    type Output = GetLogsOutput;
}

// get_log_services
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogServicesArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogServicesOutput {
    pub projects: Vec<DockerLogProject>,
}

pub struct GetLogServices;
impl OrcaToolDef for GetLogServices {
    const NAME: &'static str = "get_log_services";
    const DESCRIPTION: &'static str = "List every docker-compose project under the rebuy root with its service states. \
         Powers the cross-project logs panel.";
    type Args = GetLogServicesArgs;
    type Output = GetLogServicesOutput;
}

// ═══════════════════════════════════════════════════════════════════════════
// Native run impls
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::docker::DockerService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn DockerService>> {
        ctx.service::<Arc<dyn DockerService>>()
    }

    #[async_trait]
    impl OrcaTool for GetDockerEngine {
        async fn run(_args: GetDockerEngineArgs, ctx: &ToolCtx) -> Result<DockerEngineStatus> {
            svc(ctx)?.engine_status().await
        }
    }

    #[async_trait]
    impl OrcaTool for StartDockerEngine {
        async fn run(
            _args: StartDockerEngineArgs,
            ctx: &ToolCtx,
        ) -> Result<StartDockerEngineOutput> {
            let output = svc(ctx)?.engine_start().await?;
            Ok(StartDockerEngineOutput { output })
        }
    }

    #[async_trait]
    impl OrcaTool for GetDockerServices {
        async fn run(args: GetDockerServicesArgs, ctx: &ToolCtx) -> Result<DockerServicesView> {
            svc(ctx)?.services(&args.path).await
        }
    }

    #[async_trait]
    impl OrcaTool for RunDockerAction {
        async fn run(args: RunDockerActionArgs, ctx: &ToolCtx) -> Result<DockerActionResult> {
            svc(ctx)?
                .action(
                    &args.project_path,
                    &args.action,
                    args.service.as_deref(),
                    args.tail,
                )
                .await
        }
    }

    #[async_trait]
    impl OrcaTool for GetLogs {
        async fn run(args: GetLogsArgs, ctx: &ToolCtx) -> Result<GetLogsOutput> {
            let tail = args.tail.unwrap_or(200);
            let output = svc(ctx)?
                .logs(&args.project, args.service.as_deref(), tail)
                .await?;
            Ok(GetLogsOutput { output })
        }
    }

    #[async_trait]
    impl OrcaTool for GetLogServices {
        async fn run(_args: GetLogServicesArgs, ctx: &ToolCtx) -> Result<GetLogServicesOutput> {
            let projects = svc(ctx)?.log_services().await?;
            Ok(GetLogServicesOutput { projects })
        }
    }
}
