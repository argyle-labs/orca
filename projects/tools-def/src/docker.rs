//! Docker domain tools — engine state, compose service listing, lifecycle
//! actions, log fetch, and the cross-project log services aggregator.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Shared row shapes ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "lowercase")]
pub enum DockerEngineKind {
    Colima,
    Desktop,
    None,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerEngineStatus {
    pub engine: DockerEngineKind,
    pub running: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerServiceRow {
    pub name: String,
    pub state: String,
    pub running: bool,
    pub health: String,
    pub ports: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DockerServicesView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    pub services: Vec<DockerServiceRow>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DockerActionResult {
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerLogProject {
    pub project: String,
    pub path: String,
    pub services: Vec<DockerServiceRow>,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetDockerEngineArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StartDockerEngineArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StartDockerEngineOutput {
    pub output: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetDockerServicesArgs {
    /// Absolute path to the docker-compose project directory.
    pub path: String,
}

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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogsOutput {
    pub output: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogServicesArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetLogServicesOutput {
    pub projects: Vec<DockerLogProject>,
}

/// Live CPU/memory stats for one running container.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerContainerStats {
    pub id: String,
    pub name: String,
    /// CPU usage as a percentage of total host capacity (all cores).
    pub cpu_percent: f64,
    /// RSS-equivalent working set in MB.
    pub mem_usage_mb: u64,
    /// Container memory limit in MB (`0` = unlimited / host RAM).
    pub mem_limit_mb: u64,
    /// Block I/O read bytes since container start.
    pub block_read_bytes: u64,
    /// Block I/O write bytes since container start.
    pub block_write_bytes: u64,
    /// Net rx bytes.
    pub net_rx_bytes: u64,
    /// Net tx bytes.
    pub net_tx_bytes: u64,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockerStatsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockerStatsOutput {
    pub containers: Vec<DockerContainerStats>,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn docker_svc(
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::docker::DockerService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::docker::DockerService>>()
}

/// Probe the local docker engine (colima | desktop | none) and whether it is running.
#[orca_tool(domain = "docker.engine", verb = "detail")]
async fn docker_engine_detail(
    _args: GetDockerEngineArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<DockerEngineStatus> {
    docker_svc(ctx)?.engine_status().await
}

/// [MUTATES STATE] Start the local docker engine. Returns the start-command output.
#[orca_tool(domain = "docker.engine", verb = "update")]
async fn docker_engine_update(
    _args: StartDockerEngineArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<StartDockerEngineOutput> {
    let output = docker_svc(ctx)?.engine_start().await?;
    Ok(StartDockerEngineOutput { output })
}

/// List the compose services under `path` with state/health/ports plus the
/// resolved compose-file path.
#[orca_tool(domain = "docker.service", verb = "list")]
async fn docker_service_list(
    args: GetDockerServicesArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<DockerServicesView> {
    docker_svc(ctx)?.services(&args.path).await
}

/// [MUTATES STATE] Run a docker-compose lifecycle action against the compose
/// project at `project_path`.
#[orca_tool(domain = "docker.service", verb = "update")]
async fn docker_service_update(
    args: RunDockerActionArgs,
    ctx: &orca_tool::ToolCtx,
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
#[orca_tool(domain = "docker.service", verb = "detail")]
async fn docker_service_detail(
    args: GetLogsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<GetLogsOutput> {
    let tail = args.tail.unwrap_or(200);
    let output = docker_svc(ctx)?
        .logs(&args.project, args.service.as_deref(), tail)
        .await?;
    Ok(GetLogsOutput { output })
}

/// List every docker-compose project under the rebuy root with its service
/// states. Powers the cross-project logs panel.
#[orca_tool(domain = "docker.service", verb = "list-logs")]
async fn docker_service_list_logs(
    _args: GetLogServicesArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<GetLogServicesOutput> {
    let projects = docker_svc(ctx)?.log_services().await?;
    Ok(GetLogServicesOutput { projects })
}

/// Live CPU + memory stats for all running containers (`docker stats --no-stream`).
/// Returns an empty list when docker is not running or no containers are up.
#[orca_tool(domain = "docker.service", verb = "list-stats")]
async fn docker_service_list_stats(
    _args: DockerStatsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<DockerStatsOutput> {
    let containers = docker_svc(ctx)?.container_stats().await?;
    Ok(DockerStatsOutput { containers })
}
