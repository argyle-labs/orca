//! Docker tool surface — flat 4-tool surface (`docker.{list, detail, update,
//! delete}`) that covers everything the previous 10-tool 3-level surface did.
//!
//! Sub-resources (services / runtimes / engine) are discriminated by the
//! `kind` field on each tool's args. Tool bodies dispatch internally and
//! return a flat output with only the populated fields.
//!
//! Pod awareness: callers pass `--peer <host>` to dispatch any of these
//! tools to a remote orca peer (universal opt-out via `local_only=true`
//! on the macro; docker tools are all opt-in to remote dispatch).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use derive::orca_tool;

use crate::{Compose, ComposeError, Engine};

// ── Resource discriminator ──────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DockerKind {
    #[default]
    Service,
    Runtime,
    Engine,
}

// ── Row shapes ──────────────────────────────────────────────────────────────

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
pub struct DockerProjectRow {
    pub project: String,
    pub path: String,
    pub services: Vec<DockerServiceRow>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DockerRuntimeRow {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DockerContainerStats {
    pub id: String,
    pub name: String,
    pub cpu_percent: f64,
    pub mem_usage_mb: u64,
    pub mem_limit_mb: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

fn map_engine(e: Engine) -> DockerEngineKind {
    match e {
        Engine::Colima => DockerEngineKind::Colima,
        Engine::Desktop => DockerEngineKind::Desktop,
        Engine::None => DockerEngineKind::None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// docker.list
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct DockerListArgs {
    /// `service` (default) | `runtime` | `engine`.
    #[cfg_attr(feature = "cli", arg(long, default_value = "service"))]
    pub kind: DockerKind,
    /// (service) Single compose project path. Mutually exclusive with `root`.
    #[cfg_attr(feature = "cli", arg(long))]
    pub path: Option<String>,
    /// (service) Scan this directory for compose projects (default `$HOME/code`).
    #[cfg_attr(feature = "cli", arg(long))]
    pub root: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockerListOutput {
    /// Populated when listing one compose project's services.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<DockerServiceRow>,
    /// Populated when scanning a root for compose projects.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<DockerProjectRow>,
    /// Populated when `kind=runtime`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runtimes: Vec<DockerRuntimeRow>,
    /// Populated when `kind=engine`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<DockerEngineStatus>,
}

/// List docker resources. Discriminated by `kind`:
/// - `service` + `path` → compose services at that project.
/// - `service` + `root` → every compose project under root with its services.
/// - `runtime` → registered docker runtimes in orca.db.
/// - `engine` → local docker engine status (colima/desktop/none + running).
#[orca_tool(domain = "docker", verb = "list")]
async fn docker_list(
    args: DockerListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockerListOutput> {
    let mut out = DockerListOutput::default();
    match args.kind {
        DockerKind::Engine => {
            let s = crate::engine::status().await;
            out.engine = Some(DockerEngineStatus {
                engine: map_engine(s.engine),
                running: s.running,
            });
        }
        DockerKind::Runtime => {
            let conn = db::open_default()?;
            out.runtimes = db::docker_runtimes::list(&conn)?
                .into_iter()
                .map(|r| DockerRuntimeRow {
                    name: r.name,
                    socket_path: r.socket_path,
                    host: r.host,
                    url: r.url,
                    enabled: r.enabled,
                })
                .collect();
        }
        DockerKind::Service => match (args.path.as_deref(), args.root.as_deref()) {
            (Some(_), Some(_)) => anyhow::bail!("pass either --path or --root, not both"),
            (Some(path), None) => {
                if let Some(compose) = Compose::find(Path::new(path)) {
                    let services = compose.services().await.map_err(anyhow::Error::from)?;
                    out.compose_file = compose.file().to_str().map(str::to_string);
                    out.services = services
                        .into_iter()
                        .map(|s| DockerServiceRow {
                            name: s.name,
                            state: s.state,
                            running: s.running,
                            health: s.health,
                            ports: s.ports,
                        })
                        .collect();
                }
            }
            (None, root_opt) => {
                let home = std::env::var("HOME").unwrap_or_default();
                let root = root_opt
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{home}/code"));
                let Ok(entries) = std::fs::read_dir(&root) else {
                    return Ok(out);
                };
                let project_dirs: Vec<PathBuf> = entries
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        (p.is_dir() && Compose::find(&p).is_some()).then_some(p)
                    })
                    .collect();
                for project_path in project_dirs {
                    let path_str = project_path.to_string_lossy().into_owned();
                    let name = project_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path_str.clone());
                    let services = match Compose::find(&project_path) {
                        None => Vec::new(),
                        Some(c) => c
                            .services()
                            .await
                            .map(|svcs| {
                                svcs.into_iter()
                                    .map(|s| DockerServiceRow {
                                        name: s.name,
                                        state: s.state,
                                        running: s.running,
                                        health: s.health,
                                        ports: s.ports,
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    };
                    out.projects.push(DockerProjectRow {
                        project: name,
                        path: path_str,
                        services,
                    });
                }
            }
        },
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// docker.detail
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct DockerDetailArgs {
    /// `service` (default) | `engine`. `runtime` detail not currently supported.
    #[cfg_attr(feature = "cli", arg(long, default_value = "service"))]
    pub kind: DockerKind,
    /// (service) Absolute path to the compose project.
    #[cfg_attr(feature = "cli", arg(long))]
    pub path: Option<String>,
    /// (service) Specific service name; omit for project-wide logs.
    #[cfg_attr(feature = "cli", arg(long))]
    pub service: Option<String>,
    /// (service) Number of log lines (default 200).
    #[cfg_attr(feature = "cli", arg(long))]
    pub tail: Option<u32>,
    /// (service) Include live container stats.
    #[cfg_attr(feature = "cli", arg(long))]
    pub stats: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockerDetailOutput {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub logs: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stats: Vec<DockerContainerStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<DockerEngineStatus>,
}

#[orca_tool(domain = "docker", verb = "detail")]
async fn docker_detail(
    args: DockerDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockerDetailOutput> {
    let mut out = DockerDetailOutput::default();
    match args.kind {
        DockerKind::Engine => {
            let s = crate::engine::status().await;
            out.engine = Some(DockerEngineStatus {
                engine: map_engine(s.engine),
                running: s.running,
            });
        }
        DockerKind::Runtime => anyhow::bail!("docker.detail for kind=runtime not supported"),
        DockerKind::Service => {
            if let Some(path) = args.path.as_deref() {
                let compose = Compose::find(Path::new(path))
                    .ok_or_else(|| anyhow::anyhow!("no compose file under {path}"))?;
                let tail = args.tail.unwrap_or(200);
                let svc = args.service.as_deref();
                let services: Vec<&str> = svc.into_iter().collect();
                out.logs = compose
                    .logs(&services, tail)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            if args.stats {
                let raw = crate::containers::live_stats().await?;
                out.stats = raw
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
            }
        }
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// docker.update
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockerUpdateArgs {
    /// `service` (default) | `runtime` | `engine`.
    #[cfg_attr(feature = "cli", arg(long, default_value = "service"))]
    pub kind: DockerKind,
    /// (engine) `start` is the only action; (service) `up`/`down`/`restart`/
    /// `start`/`stop`/`build`/`pull`/`logs`. Ignored for runtime.
    #[cfg_attr(feature = "cli", arg(long))]
    pub action: Option<String>,
    /// (service) Absolute compose project path.
    #[cfg_attr(feature = "cli", arg(long))]
    pub project_path: Option<String>,
    /// (service) Optional service to scope the action to.
    #[cfg_attr(feature = "cli", arg(long))]
    pub service: Option<String>,
    /// (service) Tail for `logs` action.
    #[cfg_attr(feature = "cli", arg(long))]
    pub tail: Option<u32>,
    /// (runtime) Name of the runtime to register.
    #[cfg_attr(feature = "cli", arg(long))]
    pub name: Option<String>,
    /// (runtime) Provide socket_path, host, or url.
    #[cfg_attr(feature = "cli", arg(long))]
    pub socket_path: Option<String>,
    #[cfg_attr(feature = "cli", arg(long))]
    pub host: Option<String>,
    #[cfg_attr(feature = "cli", arg(long))]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DockerUpdateOutput {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<String>,
}

#[orca_tool(domain = "docker", verb = "update")]
async fn docker_update(
    args: DockerUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockerUpdateOutput> {
    let mut out = DockerUpdateOutput::default();
    match args.kind {
        DockerKind::Engine => {
            let action = args.action.as_deref().unwrap_or("start");
            if action != "start" {
                anyhow::bail!("docker.update kind=engine supports action=start only");
            }
            out.output = crate::engine::start().await?;
            out.applied = Some("engine-start".into());
        }
        DockerKind::Service => {
            let project_path = args
                .project_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("project_path required for kind=service"))?;
            let action = args
                .action
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("action required for kind=service"))?;
            let compose = Compose::find(Path::new(project_path))
                .ok_or_else(|| anyhow::anyhow!("no compose file under {project_path}"))?;
            out.output = compose
                .run_action(action, args.service.as_deref(), args.tail)
                .await
                .map_err(|e| match e {
                    ComposeError::UnknownAction(a) => anyhow::anyhow!("unknown action: {a}"),
                    other => anyhow::Error::from(other),
                })?;
            out.compose_file = compose.file().to_str().map(str::to_string);
            out.applied = Some(format!("service-{action}"));
        }
        DockerKind::Runtime => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("name required for kind=runtime"))?;
            if args.socket_path.is_none() && args.host.is_none() && args.url.is_none() {
                anyhow::bail!("provide socket_path, host, or url");
            }
            let row = db::docker_runtimes::RuntimeRow {
                name: name.to_string(),
                socket_path: args.socket_path,
                host: args.host,
                url: args.url,
                enabled: true,
            };
            let conn = db::open_default()?;
            db::docker_runtimes::upsert(&conn, &row)?;
            out.applied = Some(format!("runtime-upserted:{name}"));
        }
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// docker.delete
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DockerDeleteArgs {
    /// `runtime` is the only kind currently supported.
    #[cfg_attr(feature = "cli", arg(long, default_value = "runtime"))]
    #[serde(default)]
    pub kind: DockerKind,
    /// Name of the runtime to remove.
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockerDeleteOutput {
    pub name: String,
    pub changed: bool,
}

#[orca_tool(domain = "docker", verb = "delete")]
async fn docker_delete(
    args: DockerDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<DockerDeleteOutput> {
    match args.kind {
        DockerKind::Runtime => {
            let conn = db::open_default()?;
            let changed = db::docker_runtimes::remove(&conn, &args.name)?;
            Ok(DockerDeleteOutput {
                name: args.name,
                changed,
            })
        }
        other => anyhow::bail!("docker.delete kind={other:?} not supported"),
    }
}
