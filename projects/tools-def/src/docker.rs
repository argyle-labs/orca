//! Docker domain tools — engine state, compose service listing, lifecycle
//! actions, log fetch, and the cross-project log services aggregator.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

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

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetDockerEngineArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StartDockerEngineArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StartDockerEngineOutput {
    pub output: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetDockerServicesArgs {
    /// Absolute path to the docker-compose project directory.
    pub path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogServicesArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogServicesOutput {
    pub projects: Vec<DockerLogProject>,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn docker_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::docker::DockerService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::docker::DockerService>>()
}

/// Probe the local docker engine (colima | desktop | none) and whether it is running.
#[orca_tool(domain = "docker", verb = "engine")]
async fn get_docker_engine(
    _args: GetDockerEngineArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DockerEngineStatus> {
    docker_svc(ctx)?.engine_status().await
}

/// [MUTATES STATE] Start the local docker engine. Returns the start-command output.
#[orca_tool(domain = "docker", verb = "engine-start")]
async fn start_docker_engine(
    _args: StartDockerEngineArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<StartDockerEngineOutput> {
    let output = docker_svc(ctx)?.engine_start().await?;
    Ok(StartDockerEngineOutput { output })
}

/// List the compose services under `path` with state/health/ports plus the
/// resolved compose-file path.
#[orca_tool(domain = "docker", verb = "services")]
async fn get_docker_services(
    args: GetDockerServicesArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DockerServicesView> {
    docker_svc(ctx)?.services(&args.path).await
}

/// [MUTATES STATE] Run a docker-compose lifecycle action against the compose
/// project at `project_path`.
#[orca_tool(domain = "docker", verb = "action")]
async fn run_docker_action(
    args: RunDockerActionArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DockerActionResult> {
    docker_svc(ctx)?
        .action(
            &args.project_path,
            &args.action,
            args.service.as_deref(),
            args.tail,
        )
        .await
}

/// Read docker-compose logs from the project at `project` (optionally scoped
/// to a service).
#[orca_tool(domain = "docker", verb = "logs")]
async fn get_logs(
    args: GetLogsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetLogsOutput> {
    let tail = args.tail.unwrap_or(200);
    let output = docker_svc(ctx)?
        .logs(&args.project, args.service.as_deref(), tail)
        .await?;
    Ok(GetLogsOutput { output })
}

/// List every docker-compose project under the rebuy root with its service
/// states. Powers the cross-project logs panel.
#[orca_tool(domain = "docker", verb = "log-services")]
async fn get_log_services(
    _args: GetLogServicesArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetLogServicesOutput> {
    let projects = docker_svc(ctx)?.log_services().await?;
    Ok(GetLogServicesOutput { projects })
}
