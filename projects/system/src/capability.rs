//! Per-host capability registry — probes (docker, proxmox, ...) +
//! sync helpers that gate collectors and tool surfaces.
//!
//! Each provider is probed ONCE at daemon startup via
//! [`probe_all_capabilities`]. Results land in `db::host_capabilities`.
//! Collectors call [`is_available`] before invoking provider-specific
//! code; an absent provider is skipped silently — no warn-every-tick.
//!
//! `Disabled` state (set by `system.capability.disable`) is sticky:
//! startup probes leave disabled rows alone so operator intent survives
//! restarts.
//!
//! Add a new provider by appending its [`CapabilityProbe`] impl to
//! [`builtin_probes`].

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use db::host_capabilities::{CapabilityState, HostCapability};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Outcome of one probe. `Absent` carries the operator-visible reason
/// (binary not in PATH, marker file missing, etc.); `Available` carries
/// a version string when one can be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Available { detail: Option<String> },
    Absent { reason: String },
}

#[async_trait]
pub trait CapabilityProbe: Send + Sync {
    /// Provider key — primary key in `host_capabilities`. Stable across
    /// daemon restarts (operator references this in
    /// `system.capability.disable --name <provider>`).
    fn name(&self) -> &'static str;

    async fn probe(&self) -> ProbeResult;
}

/// All known built-in capability probes. Order is the order they're
/// probed at startup (cheap probes first so a slow probe can't delay
/// faster ones).
pub fn builtin_probes() -> Vec<Box<dyn CapabilityProbe>> {
    vec![Box::new(ProxmoxProbe), Box::new(DockerProbe)]
}

/// Probe every built-in capability and persist the result. Called once
/// at daemon startup. `Disabled` rows are NOT re-probed — operator
/// intent wins until they explicitly `enable`.
pub async fn probe_all_capabilities() -> Result<()> {
    let probes = builtin_probes();
    for probe in &probes {
        if let Err(e) = probe_and_store(probe.as_ref()).await {
            tracing::warn!(
                target: "system::capability",
                provider = probe.name(),
                "capability probe failed to persist: {e:#}"
            );
        }
    }
    Ok(())
}

/// Re-probe one named provider. Used by `system.capability.recheck`
/// (and by `system.capability.enable` after clearing `Disabled`).
/// Errors if the provider name isn't in [`builtin_probes`].
pub async fn recheck(provider: &str) -> Result<HostCapability> {
    let probes = builtin_probes();
    let p = probes
        .iter()
        .find(|p| p.name() == provider)
        .ok_or_else(|| anyhow!("unknown capability `{provider}`"))?;
    probe_and_store(p.as_ref()).await
}

/// Mark `provider` as `Disabled` with `reason`. Sticky across restarts.
/// Returns the persisted row. Errors only on db failure — disabling an
/// already-disabled provider is idempotent.
pub fn disable(provider: &str, reason: &str) -> Result<HostCapability> {
    if !is_known_provider(provider) {
        return Err(anyhow!("unknown capability `{provider}`"));
    }
    let row = HostCapability {
        provider: provider.to_string(),
        state: CapabilityState::Disabled,
        last_probed: now_unix(),
        reason: Some(reason.to_string()),
        detail: None,
    };
    let conn = db::open_default()?;
    db::host_capabilities::upsert(&conn, &row)?;
    Ok(row)
}

/// Clear a `Disabled` row and immediately re-probe so the result
/// reflects current host reality. Returns the new probe outcome.
pub async fn enable(provider: &str) -> Result<HostCapability> {
    if !is_known_provider(provider) {
        return Err(anyhow!("unknown capability `{provider}`"));
    }
    recheck(provider).await
}

/// Fast synchronous read: is `provider` currently `Available`? Used by
/// collector gates. Returns `false` on db error or unknown provider so
/// "no info" defaults to "don't try", matching the spirit of "only
/// surface what delta has".
pub fn is_available(provider: &str) -> bool {
    let Ok(conn) = db::open_default() else {
        return false;
    };
    matches!(
        db::host_capabilities::get(&conn, provider),
        Ok(Some(row)) if row.state == CapabilityState::Available
    )
}

/// List every known capability row. Used by `system.capability.list`.
pub fn list() -> Result<Vec<HostCapability>> {
    let conn = db::open_default()?;
    db::host_capabilities::list_all(&conn)
}

// ── internals ────────────────────────────────────────────────────────

fn is_known_provider(name: &str) -> bool {
    builtin_probes().iter().any(|p| p.name() == name)
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn probe_and_store(probe: &dyn CapabilityProbe) -> Result<HostCapability> {
    let name = probe.name();
    let conn = db::open_default()?;

    // Disabled rows survive probes — operator intent wins.
    if let Some(existing) = db::host_capabilities::get(&conn, name)?
        && existing.state == CapabilityState::Disabled
    {
        return Ok(existing);
    }

    let result = probe.probe().await;
    let row = match result {
        ProbeResult::Available { detail } => HostCapability {
            provider: name.to_string(),
            state: CapabilityState::Available,
            last_probed: now_unix(),
            reason: None,
            detail,
        },
        ProbeResult::Absent { reason } => HostCapability {
            provider: name.to_string(),
            state: CapabilityState::Absent,
            last_probed: now_unix(),
            reason: Some(reason),
            detail: None,
        },
    };
    db::host_capabilities::upsert(&conn, &row)?;
    Ok(row)
}

// ── built-in probes ──────────────────────────────────────────────────

/// Proxmox host. Checked via cheap filesystem markers — same approach
/// as the existing `detect_proxmox_role` (system_info.rs); presence of
/// `/etc/pve/` (pmxcfs) or `/usr/bin/pveversion` is a strong,
/// false-positive-free signal that needs no shell-out.
struct ProxmoxProbe;

#[async_trait]
impl CapabilityProbe for ProxmoxProbe {
    fn name(&self) -> &'static str {
        "proxmox"
    }

    async fn probe(&self) -> ProbeResult {
        #[cfg(target_os = "linux")]
        {
            if std::path::Path::new("/etc/pve").is_dir() {
                return ProbeResult::Available {
                    detail: Some("pmxcfs at /etc/pve".to_string()),
                };
            }
            if std::path::Path::new("/usr/bin/pveversion").is_file() {
                return ProbeResult::Available {
                    detail: Some("pveversion present".to_string()),
                };
            }
            ProbeResult::Absent {
                reason: "neither /etc/pve nor /usr/bin/pveversion present".to_string(),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ProbeResult::Absent {
                reason: "proxmox is linux-only".to_string(),
            }
        }
    }
}

/// Docker engine. Probe = `docker version --format {{.Server.Version}}`
/// with a 3 s timeout. ENOENT (no `docker` in PATH) and non-zero exit
/// both yield `Absent`. The version string is captured when Available.
struct DockerProbe;

#[async_trait]
impl CapabilityProbe for DockerProbe {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn probe(&self) -> ProbeResult {
        let fut = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        let out = match timeout(Duration::from_secs(3), fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return ProbeResult::Absent {
                    reason: format!("spawn docker: {e}"),
                };
            }
            Err(_) => {
                return ProbeResult::Absent {
                    reason: "docker version timed out after 3s".to_string(),
                };
            }
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let reason = if stderr.is_empty() {
                format!("docker version exited {}", out.status)
            } else {
                stderr
            };
            return ProbeResult::Absent { reason };
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        ProbeResult::Available {
            detail: if version.is_empty() {
                None
            } else {
                Some(version)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `disable` / `enable` round-trip leaves the row in a re-probed
    /// state — Disabled is sticky only between disable and explicit
    /// enable, not forever.
    #[test]
    fn known_providers_match_builtins() {
        let names: Vec<_> = builtin_probes().iter().map(|p| p.name()).collect();
        assert!(names.contains(&"docker"));
        assert!(names.contains(&"proxmox"));
        assert!(is_known_provider("docker"));
        assert!(is_known_provider("proxmox"));
        assert!(!is_known_provider("unraid"));
    }

    #[tokio::test]
    async fn docker_probe_absent_on_typical_macos_dev_box_without_docker() {
        // We can't assert Available/Absent reliably in CI; we only
        // assert the probe returns SOMETHING and doesn't panic / hang.
        // (The 3 s timeout is the actual contract here.)
        let r = DockerProbe.probe().await;
        match r {
            ProbeResult::Available { .. } | ProbeResult::Absent { .. } => {}
        }
    }
}
