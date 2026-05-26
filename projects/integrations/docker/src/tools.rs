//! Docker domain tools — engine state, compose service listing, lifecycle
//! actions, log fetch, and the cross-project log services aggregator.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use crate::{Compose, ComposeError, Engine};
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

use orca_macro::orca_tool;

#[cfg(feature = "native")]
fn map_engine(e: Engine) -> DockerEngineKind {
    match e {
        Engine::Colima => DockerEngineKind::Colima,
        Engine::Desktop => DockerEngineKind::Desktop,
        Engine::None => DockerEngineKind::None,
    }
}

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

/// Probe the local docker engine (colima | desktop | none) and whether it is running.
#[orca_tool(domain = "docker.engine", verb = "detail")]
async fn docker_engine_detail(
    _args: GetDockerEngineArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DockerEngineStatus> {
    let s = crate::engine::status().await;
    Ok(DockerEngineStatus {
        engine: map_engine(s.engine),
        running: s.running,
    })
}

/// [MUTATES STATE] Start the local docker engine. Returns the start-command output.
#[orca_tool(domain = "docker.engine", verb = "update")]
async fn docker_engine_update(
    _args: StartDockerEngineArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<StartDockerEngineOutput> {
    let output = crate::engine::start().await?;
    Ok(StartDockerEngineOutput { output })
}

/// List the compose services under `path` with state/health/ports plus the
/// resolved compose-file path.
#[orca_tool(domain = "docker.service", verb = "list")]
async fn docker_service_list(
    args: GetDockerServicesArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DockerServicesView> {
    let Some(compose) = Compose::find(Path::new(&args.path)) else {
        return Ok(DockerServicesView {
            compose_file: None,
            services: Vec::new(),
        });
    };
    let services = compose.services().await.map_err(anyhow::Error::from)?;
    Ok(DockerServicesView {
        compose_file: compose.file().to_str().map(str::to_string),
        services: services
            .into_iter()
            .map(|s| DockerServiceRow {
                name: s.name,
                state: s.state,
                running: s.running,
                health: s.health,
                ports: s.ports,
            })
            .collect(),
    })
}

/// [MUTATES STATE] Run a docker-compose lifecycle action against the compose
/// project at `project_path`.
#[orca_tool(domain = "docker.service", verb = "update")]
async fn docker_service_update(
    args: RunDockerActionArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DockerActionResult> {
    let compose = Compose::find(Path::new(&args.project_path))
        .ok_or_else(|| anyhow::anyhow!("no compose file under {}", args.project_path))?;
    let output = compose
        .run_action(&args.action, args.service.as_deref(), args.tail)
        .await
        .map_err(|e| match e {
            ComposeError::UnknownAction(a) => anyhow::anyhow!("unknown action: {a}"),
            other => anyhow::Error::from(other),
        })?;
    Ok(DockerActionResult {
        output,
        compose_file: compose.file().to_str().map(str::to_string),
    })
}

/// Read docker-compose logs from the project at `project` (optionally scoped
/// to a service).
#[orca_tool(domain = "docker.service", verb = "detail")]
async fn docker_service_detail(
    args: GetLogsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetLogsOutput> {
    let tail = args.tail.unwrap_or(200);
    let compose = Compose::find(Path::new(&args.project))
        .ok_or_else(|| anyhow::anyhow!("no compose file under {}", args.project))?;
    let svc = args.service.as_deref();
    let services: Vec<&str> = svc.into_iter().collect();
    let output = compose
        .logs(&services, tail)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(GetLogsOutput { output })
}

/// List every docker-compose project under the rebuy root with its service
/// states. Powers the cross-project logs panel.
#[orca_tool(domain = "docker.service", verb = "list-logs")]
async fn docker_service_list_logs(
    _args: GetLogServicesArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetLogServicesOutput> {
    let home = std::env::var("HOME").unwrap_or_default();
    let rebuy_root = std::env::var("REBUY_ROOT").unwrap_or_else(|_| format!("{home}/code/rebuy"));

    let entries = match std::fs::read_dir(&rebuy_root) {
        Ok(e) => e,
        Err(_) => return Ok(GetLogServicesOutput { projects: Vec::new() }),
    };

    let project_dirs: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.is_dir() && Compose::find(&p).is_some() {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    let mut out = Vec::with_capacity(project_dirs.len());
    for project_path in project_dirs {
        let path_str = project_path.to_string_lossy().into_owned();
        let name = project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.clone());

        let services = match Compose::find(&project_path) {
            None => Vec::new(),
            Some(c) => match c.services().await {
                Ok(s) => s
                    .into_iter()
                    .map(|s| DockerServiceRow {
                        name: s.name,
                        state: s.state,
                        running: s.running,
                        health: s.health,
                        ports: s.ports,
                    })
                    .collect(),
                Err(_) => Vec::new(),
            },
        };

        out.push(DockerLogProject {
            project: name,
            path: path_str,
            services,
        });
    }

    Ok(GetLogServicesOutput { projects: out })
}

/// Live CPU + memory stats for all running containers (`docker stats --no-stream`).
/// Returns an empty list when docker is not running or no containers are up.
#[orca_tool(domain = "docker.service", verb = "list-stats")]
async fn docker_service_list_stats(
    _args: DockerStatsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DockerStatsOutput> {
    let raw = crate::containers::live_stats().await?;
    let containers = raw
        .into_iter()
        .map(|s| DockerContainerStats {
            id: s.id,
            name: s.name,
            cpu_percent: s.cpu_percent,
            mem_usage_mb: s.mem_usage_mb,
            mem_limit_mb: s.mem_limit_mb,
            block_read_bytes: s.block_read_bytes,
            block_write_bytes: s.block_write_bytes,
            net_rx_bytes: s.net_rx_bytes,
            net_tx_bytes: s.net_tx_bytes,
        })
        .collect();
    Ok(DockerStatsOutput { containers })
}
