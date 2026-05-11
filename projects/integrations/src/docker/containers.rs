//! Container-level ops via the docker CLI on top of [`crate::run`].
//! Swap to bollard when streaming/events land.
// serde_json::Value is intentional: `docker inspect` returns a large,
// version-dependent JSON array; callers pick the fields they need.
#![allow(clippy::disallowed_types)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSummary {
    #[serde(default, rename = "ID", alias = "Id")]
    pub id: String,
    #[serde(default, rename = "Names")]
    pub names: String,
    #[serde(default, rename = "Image")]
    pub image: String,
    #[serde(default, rename = "Status")]
    pub status: String,
    #[serde(default, rename = "State")]
    pub state: String,
    #[serde(default, rename = "Ports")]
    pub ports: String,
}

/// `docker ps --format '{{json .}}'`. When `all=true`, includes stopped.
pub async fn list(all: bool) -> anyhow::Result<Vec<ContainerSummary>> {
    let mut args: Vec<&str> = vec!["ps", "--format", "{{json .}}"];
    if all {
        args.push("--all");
    }
    let out = super::run(&args, None).await?;
    let mut result = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<ContainerSummary>(line) {
            Ok(c) => result.push(c),
            Err(_) => continue,
        }
    }
    Ok(result)
}

/// `docker logs --tail <n> <container>`. Returns combined stdout+stderr.
/// Default tail is 100; capped to 10_000 to mirror the former plugin schema.
pub async fn logs(container: &str, tail: Option<u32>) -> anyhow::Result<String> {
    if container.is_empty() {
        anyhow::bail!("missing 'container'");
    }
    let n = tail.unwrap_or(100).clamp(1, 10_000);
    let n_str = n.to_string();
    super::run(&["logs", "--tail", &n_str, container], None).await
}

/// `docker start <container>`.
pub async fn start(container: &str) -> anyhow::Result<String> {
    action(container, "start").await
}

/// `docker stop <container>`.
pub async fn stop(container: &str) -> anyhow::Result<String> {
    action(container, "stop").await
}

/// `docker restart <container>`.
pub async fn restart(container: &str) -> anyhow::Result<String> {
    action(container, "restart").await
}

/// `docker inspect <container>` — returns the parsed JSON (an array of one).
pub async fn inspect(container: &str) -> anyhow::Result<Value> {
    if container.is_empty() {
        anyhow::bail!("missing 'container'");
    }
    let out = super::run(&["inspect", container], None).await?;
    Ok(serde_json::from_str(&out)?)
}

async fn action(container: &str, op: &str) -> anyhow::Result<String> {
    if container.is_empty() {
        anyhow::bail!("missing 'container'");
    }
    super::run(&[op, container], None).await
}
