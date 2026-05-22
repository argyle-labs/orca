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

/// One row from `docker stats --no-stream --format '{{json .}}'`.
/// Field names match docker's Go template output keys exactly.
#[derive(Debug, Deserialize)]
struct RawStatsRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu_perc: String,
    #[serde(rename = "MemUsage")]
    mem_usage: String,
    #[serde(rename = "BlockIO")]
    block_io: String,
    #[serde(rename = "NetIO")]
    net_io: String,
}

/// Live CPU/memory stats for all running containers.
pub struct ContainerLiveStats {
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

/// Parse `"1.23%"` → 1.23, returning 0.0 on error.
fn parse_percent(s: &str) -> f64 {
    s.trim_end_matches('%').trim().parse::<f64>().unwrap_or(0.0)
}

/// Parse memory strings like `"1.23GiB / 15.53GiB"` or `"512MiB / 0B"`.
/// Returns `(used_mb, limit_mb)`.
fn parse_mem_pair(s: &str) -> (u64, u64) {
    let mut parts = s.splitn(2, '/');
    let used = parse_size_to_mb(parts.next().unwrap_or("").trim());
    let limit = parse_size_to_mb(parts.next().unwrap_or("").trim());
    (used, limit)
}

/// Parse `"1.5GiB"`, `"512MiB"`, `"2.3kB"`, `"0B"` → MB (truncated).
fn parse_size_to_mb(s: &str) -> u64 {
    let s = s.trim();
    // suffixes in order of longest match first
    for (suffix, factor_bytes) in &[
        ("GiB", 1_073_741_824u64),
        ("MiB", 1_048_576u64),
        ("KiB", 1_024u64),
        ("GB", 1_000_000_000u64),
        ("MB", 1_000_000u64),
        ("kB", 1_000u64),
        ("B", 1u64),
    ] {
        if let Some(num) = s.strip_suffix(suffix) {
            let v: f64 = num.trim().parse().unwrap_or(0.0);
            return (v * (*factor_bytes as f64) / 1_048_576.0) as u64;
        }
    }
    0
}

/// Parse IO pairs like `"1.23MB / 456kB"` → `(read_bytes, write_bytes)`.
fn parse_io_pair(s: &str) -> (u64, u64) {
    let mut parts = s.splitn(2, '/');
    let r = parse_size_to_bytes(parts.next().unwrap_or("").trim());
    let w = parse_size_to_bytes(parts.next().unwrap_or("").trim());
    (r, w)
}

fn parse_size_to_bytes(s: &str) -> u64 {
    let s = s.trim();
    for (suffix, factor) in &[
        ("GiB", 1_073_741_824u64),
        ("MiB", 1_048_576u64),
        ("KiB", 1_024u64),
        ("GB", 1_000_000_000u64),
        ("MB", 1_000_000u64),
        ("kB", 1_000u64),
        ("B", 1u64),
    ] {
        if let Some(num) = s.strip_suffix(suffix) {
            let v: f64 = num.trim().parse().unwrap_or(0.0);
            return (v * (*factor as f64)) as u64;
        }
    }
    0
}

/// `docker stats --no-stream --format '{{json .}}'` for all running containers.
pub async fn live_stats() -> anyhow::Result<Vec<ContainerLiveStats>> {
    let out = super::run(&["stats", "--no-stream", "--format", "{{json .}}"], None).await?;

    let mut result = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(row) = serde_json::from_str::<RawStatsRow>(line) else {
            continue;
        };
        let cpu_percent = parse_percent(&row.cpu_perc);
        let (mem_usage_mb, mem_limit_mb) = parse_mem_pair(&row.mem_usage);
        let (block_read_bytes, block_write_bytes) = parse_io_pair(&row.block_io);
        let (net_rx_bytes, net_tx_bytes) = parse_io_pair(&row.net_io);
        result.push(ContainerLiveStats {
            id: row.id,
            name: row.name,
            cpu_percent,
            mem_usage_mb,
            mem_limit_mb,
            block_read_bytes,
            block_write_bytes,
            net_rx_bytes,
            net_tx_bytes,
        });
    }
    Ok(result)
}
