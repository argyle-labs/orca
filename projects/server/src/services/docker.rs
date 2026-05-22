//! Server-side impl of `DockerService` — engine probing, compose project
//! services + lifecycle actions, log fetch, and the cross-project log
//! aggregator. Backed by the `orca_integrations::docker` crate.

use anyhow::Result;
use async_trait::async_trait;
use orca_integrations::docker::{self, Compose, ComposeError, Engine};
use orca_tools_def::docker::{
    DockerActionResult, DockerContainerStats, DockerEngineKind, DockerEngineStatus,
    DockerLogProject, DockerServiceRow, DockerServicesView,
};
use orca_tools_def::services::docker::DockerService;
use std::path::{Path, PathBuf};

pub struct ServerDocker;

fn map_engine(e: Engine) -> DockerEngineKind {
    match e {
        Engine::Colima => DockerEngineKind::Colima,
        Engine::Desktop => DockerEngineKind::Desktop,
        Engine::None => DockerEngineKind::None,
    }
}

#[async_trait]
impl DockerService for ServerDocker {
    async fn engine_status(&self) -> Result<DockerEngineStatus> {
        let s = docker::engine::status().await;
        Ok(DockerEngineStatus {
            engine: map_engine(s.engine),
            running: s.running,
        })
    }

    async fn engine_start(&self) -> Result<String> {
        docker::engine::start().await
    }

    async fn services(&self, project_path: &str) -> Result<DockerServicesView> {
        let Some(compose) = Compose::find(Path::new(project_path)) else {
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

    async fn action(
        &self,
        project_path: &str,
        action: &str,
        service: Option<&str>,
        tail: Option<u32>,
    ) -> Result<DockerActionResult> {
        let compose = Compose::find(Path::new(project_path))
            .ok_or_else(|| anyhow::anyhow!("no compose file under {project_path}"))?;
        let output = compose
            .run_action(action, service, tail)
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

    async fn logs(&self, project_path: &str, service: Option<&str>, tail: u32) -> Result<String> {
        let compose = Compose::find(Path::new(project_path))
            .ok_or_else(|| anyhow::anyhow!("no compose file under {project_path}"))?;
        let services: Vec<&str> = service.into_iter().collect();
        compose
            .logs(&services, tail)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn container_stats(&self) -> Result<Vec<DockerContainerStats>> {
        let raw = docker::containers::live_stats().await?;
        Ok(raw
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
            .collect())
    }

    async fn log_services(&self) -> Result<Vec<DockerLogProject>> {
        let home = std::env::var("HOME").unwrap_or_default();
        let rebuy_root =
            std::env::var("REBUY_ROOT").unwrap_or_else(|_| format!("{home}/code/rebuy"));

        let entries = match std::fs::read_dir(&rebuy_root) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
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

        Ok(out)
    }
}
