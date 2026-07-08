//! Background-refreshed cross-platform system snapshot powering
//! `system.runtime-spec.system` (and therefore `pod.list[].system`).
//!
//! Collection runs every 30s in a background task spawned at server start.
//! `current()` returns the most recent snapshot — fast (<1µs lock-free read),
//! never blocks on sysinfo. Bootstrapping callers that race the first refresh
//! get `None`; the first snapshot lands ~immediately after `spawn_refresher`.
//!
//! Relocated from `server::system_info` in slice A3.

pub mod history;
pub mod labels;
pub mod metrics;
pub mod system_type;

use crate::system_info_types::{GpuInfo, NetIfaceDto, SystemInfoReport};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use sysinfo::{Disks, Networks, Pid, ProcessRefreshKind, RefreshKind, System};

/// In-memory cache refresh interval. Tight enough that mount-state and
/// other LAN-visible changes reflect within the ≤2s bar set by
/// [[project-polling-rate-too-slow]] — LAN bandwidth is not the constraint
/// and a ~50-150ms scan at this cadence is <8% steady CPU. DB persistence
/// runs on its own cadence — see `server::host_status_writer`.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Topology-claim refresh interval. Claims involve remote proxmox API
/// fan-out (one call per guest) so they can't run at REFRESH_INTERVAL —
/// that would hammer the API at ~20 calls/sec on a 40-guest fleet. VMs and
/// LXCs don't churn fast enough to need 2s freshness; 15s keeps the
/// inference tree fresh enough for the UI without overloading proxmox.
const CLAIMS_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// Default ceiling (MiB) for this process's own RSS. A breach is logged at
/// `warn` once per refresh tick so a slow leak in orca surfaces in the daemon
/// log before it OOMs the box. Overridable via `ORCA_RSS_CEILING_MB`; set to
/// `0` to disable the check entirely.
const DEFAULT_RSS_CEILING_MB: u64 = 1024;

/// Resolve the RSS ceiling, honoring `ORCA_RSS_CEILING_MB`. A value of `0`
/// (default or override) means "no ceiling" and yields `None`.
fn rss_ceiling_mb() -> Option<u64> {
    let limit = std::env::var("ORCA_RSS_CEILING_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RSS_CEILING_MB);
    (limit > 0).then_some(limit)
}

/// Pure predicate: does the report's `process_rss_mb` exceed `limit`? Returns
/// `false` when RSS is unknown (no false alarm on a missing reading) — only a
/// concrete reading strictly above the limit trips it.
fn rss_exceeds(report: &SystemInfoReport, limit: u64) -> bool {
    matches!(report.process_rss_mb, Some(rss) if rss > limit)
}

static CACHE: OnceLock<Mutex<Option<Arc<SystemInfoReport>>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<Arc<SystemInfoReport>>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Most-recent snapshot. `None` only before the first refresh completes.
pub fn current() -> Option<Arc<SystemInfoReport>> {
    cache().lock().ok().and_then(|g| g.clone())
}

/// Cached snapshot if available; otherwise collect synchronously and cache.
/// Use for short-lived processes (CLI invocations) that don't run the
/// background refresher. Caches the first collection so repeated CLI calls
/// in the same process don't re-pay the ~50-150ms scan.
pub fn current_or_collect() -> Arc<SystemInfoReport> {
    if let Some(s) = current() {
        return s;
    }
    let snap = Arc::new(collect_blocking());
    if let Ok(mut g) = cache().lock() {
        *g = Some(snap.clone());
    }
    snap
}

/// Spawn the background refresher. Idempotent: subsequent calls do nothing.
///
/// Keeps a single `sysinfo::System` alive between ticks so CPU usage is
/// measured as a delta between refreshes rather than always returning 0 %.
pub fn spawn_refresher() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut sys = System::new_with_specifics(
            RefreshKind::new()
                .with_memory(sysinfo::MemoryRefreshKind::everything())
                .with_cpu(sysinfo::CpuRefreshKind::everything())
                .with_processes(ProcessRefreshKind::everything()),
        );
        // Prime first tick — CPU usage will be 0 on this pass.
        sys.refresh_memory();
        sys.refresh_cpu_all();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        let shutdown = crate::periodic::shutdown_token();
        // Claim collection involves remote proxmox API calls; cache between
        // ticks and only refresh on its own slower cadence. Tracked as a
        // tokio Instant so the first iteration always populates.
        let mut cached_claims: Vec<contract::TopologyClaim> = Vec::new();
        let mut claims_last_refresh: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(REFRESH_INTERVAL) => {}
                _ = shutdown.cancelled() => return,
            }
            sys.refresh_memory();
            sys.refresh_cpu_all();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

            let gpus = collect_gpus().await;
            let need_claims_refresh = claims_last_refresh
                .map(|t| t.elapsed() >= CLAIMS_REFRESH_INTERVAL)
                .unwrap_or(true);
            if need_claims_refresh {
                cached_claims = crate::topology::collect_claims().await;
                claims_last_refresh = Some(tokio::time::Instant::now());
            }
            let mut snap = snapshot_from_sys(&sys, gpus);
            snap.claims = cached_claims.clone();
            if let Some(limit) = rss_ceiling_mb()
                && rss_exceeds(&snap, limit)
            {
                tracing::warn!(
                    rss_mb = snap.process_rss_mb,
                    ceiling_mb = limit,
                    "orca process RSS exceeds ceiling — possible leak"
                );
            }
            if let Some(point) = history::point_from(&snap) {
                history::append(&point);
            }
            snap.history = history::read_tail(720);
            if let Ok(mut g) = cache().lock() {
                *g = Some(Arc::new(snap));
            }
        }
    });
}

/// Synchronously collect a fresh snapshot (no prior `System` state — CPU
/// usage will be 0). Used by tests and by `current_or_collect` for CLI paths.
pub fn collect_blocking() -> SystemInfoReport {
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_memory(sysinfo::MemoryRefreshKind::everything())
            .with_cpu(sysinfo::CpuRefreshKind::everything())
            .with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_memory();
    sys.refresh_cpu_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    snapshot_from_sys(&sys, vec![])
}

/// Build a `SystemInfoReport` from a live (already-refreshed) `System`.
/// `gpus` is pre-collected by the async caller so this stays sync.
fn snapshot_from_sys(sys: &System, gpus: Vec<GpuInfo>) -> SystemInfoReport {
    let (virt, dmi_vendor, dmi_product) = detect_virtualization();
    let mut report = SystemInfoReport {
        snapshot_at_unix: Some(chrono::Utc::now().timestamp()),
        arch: Some(std::env::consts::ARCH.to_string()),
        os_name: System::name(),
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        distro: System::long_os_version().filter(|s| !s.is_empty()),
        hostname: System::host_name(),
        boot_time_unix: Some(System::boot_time() as i64),
        system_uptime_secs: Some(System::uptime()),
        virtualization: virt,
        dmi_vendor,
        dmi_product,
        proxmox_role: detect_proxmox_role(),
        cluster: detect_pve_cluster(),
        gpus,
        ..Default::default()
    };

    report.cpu_logical = Some(sys.cpus().len() as u32);
    report.cpu_physical = sys.physical_core_count().map(|c| c as u32);
    if let Some(c) = sys.cpus().first() {
        report.cpu_model = Some(c.brand().to_string());
    }
    // global_cpu_usage() requires two refreshes (delta); first call → 0.
    let usage = sys.global_cpu_usage();
    if usage > 0.0 {
        report.cpu_usage_percent = Some(usage);
    }

    let total_mem = sys.total_memory();
    let avail_mem = sys.available_memory();
    report.mem_total_mb = Some(total_mem / 1024 / 1024);
    report.mem_used_mb = Some(total_mem.saturating_sub(avail_mem) / 1024 / 1024);
    report.mem_available_mb = Some(avail_mem / 1024 / 1024);

    let total_swap = sys.total_swap();
    report.swap_total_mb = Some(total_swap / 1024 / 1024);
    report.swap_used_mb = Some(total_swap.saturating_sub(sys.free_swap()) / 1024 / 1024);

    let la = System::load_average();
    // Windows reports zeros for the load average; treat that as "absent".
    if la.one > 0.0 || la.five > 0.0 || la.fifteen > 0.0 {
        report.load_avg_1 = Some(la.one);
        report.load_avg_5 = Some(la.five);
        report.load_avg_15 = Some(la.fifteen);
    }

    let pid = std::process::id();
    report.process_pid = Some(pid);
    if let Some(p) = sys.process(Pid::from_u32(pid)) {
        let start = p.start_time() as i64;
        report.process_started_at_unix = Some(start);
        report.process_uptime_secs = Some(p.run_time());
        report.process_rss_mb = Some(p.memory() / 1024 / 1024);
        report.process_threads = p.tasks().map(|t| t.len() as u32);
    }

    // Top 10 processes by CPU. sysinfo reports cpu_usage() as 0..=100 per
    // logical core (i.e. up to 100*N_cores total) — normalise to a single
    // 0..=100 scale for consistent rendering across hosts with different
    // core counts.
    let cores = sys.cpus().len().max(1) as f32;
    let mut procs: Vec<crate::system_info_types::TopProcess> = sys
        .processes()
        .values()
        .map(|p| crate::system_info_types::TopProcess {
            pid: p.pid().as_u32(),
            name: p.name().to_string_lossy().to_string(),
            cpu_percent: p.cpu_usage() / cores,
            mem_mb: p.memory() / 1024 / 1024,
        })
        .collect();
    procs.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    procs.truncate(10);
    report.top_processes = procs;

    // Storage — the filesystem hosting ~/.orca. Pick the longest mount-point
    // prefix so we report the right volume on hosts with separate /home or
    // /var partitions.
    let orca_dir = orca_dir();
    if let Some(ref dir) = orca_dir {
        report.orca_dir = Some(dir.display().to_string());
        let disks = Disks::new_with_refreshed_list();
        let mut best: Option<&sysinfo::Disk> = None;
        let mut best_len = 0usize;
        for d in disks.list() {
            let mp = d.mount_point();
            if dir.starts_with(mp) {
                let l = mp.as_os_str().len();
                if l > best_len {
                    best_len = l;
                    best = Some(d);
                }
            }
        }
        if let Some(d) = best {
            report.orca_fs_total_gb = Some(d.total_space() / 1024 / 1024 / 1024);
            report.orca_fs_avail_gb = Some(d.available_space() / 1024 / 1024 / 1024);
        }
    }

    report.docker_present = Some(which("docker").is_some());

    // Canonical system_type + observed capabilities. The detector takes the
    // OS-name strings sysinfo already collected above so it sees the same
    // values the rest of the report does, and probes the filesystem +
    // PATH directly for capability markers.
    let host_fs = system_type::RealHostFs;
    let sys_type = system_type::detect(&host_fs);
    report.system_type_label = Some(labels::system_type_label(&sys_type));
    report.system_type = Some(sys_type);
    let caps = system_type::detect_capabilities(&host_fs);
    report.capability_labels = caps.iter().map(|c| labels::capability_label(c)).collect();
    report.detected_capabilities = caps;

    // Precomputed percent triple — see `system_info::metrics` for semantics.
    // Computed after the underlying fields are set so the math sees the same
    // values clients would otherwise see.
    report.mem_percent = metrics::mem_percent(&report);
    report.load_percent = metrics::load_percent(&report);
    report.cpu_percent = metrics::cpu_percent(&report);

    // Network interfaces via if-addrs (already a dep). sysinfo exposes
    // interface stats but not MAC + ip list cleanly.
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        use std::collections::BTreeMap;
        let mut by_name: BTreeMap<String, NetIfaceDto> = BTreeMap::new();
        for i in ifs {
            let entry = by_name
                .entry(i.name.clone())
                .or_insert_with(|| NetIfaceDto {
                    name: i.name.clone(),
                    mac: None,
                    ipv4: Vec::new(),
                    ipv6: Vec::new(),
                    loopback: false,
                });
            entry.loopback = entry.loopback || i.is_loopback();
            match i.ip() {
                std::net::IpAddr::V4(v4) => entry.ipv4.push(v4.to_string()),
                std::net::IpAddr::V6(v6) => entry.ipv6.push(v6.to_string()),
            }
        }
        // Pull MACs from sysinfo's network view, then collapse.
        let nets = Networks::new_with_refreshed_list();
        for (name, n) in nets.iter() {
            if let Some(iface) = by_name.get_mut(name) {
                let mac = n.mac_address();
                if mac.0 != [0u8; 6] {
                    iface.mac = Some(format!(
                        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        mac.0[0], mac.0[1], mac.0[2], mac.0[3], mac.0[4], mac.0[5]
                    ));
                }
            }
        }
        // sysinfo returns all-zero MACs on alpine/musl LXC (e.g. bravo),
        // leaving every iface unpinned. Fall back to /sys/class/net for any
        // iface still missing a MAC.
        #[cfg(target_os = "linux")]
        for iface in by_name.values_mut() {
            if iface.mac.is_some() || iface.loopback {
                continue;
            }
            let path = format!("/sys/class/net/{}/address", iface.name);
            if let Ok(s) = std::fs::read_to_string(&path) {
                let trimmed = s.trim();
                if !trimmed.is_empty() && trimmed != "00:00:00:00:00:00" {
                    iface.mac = Some(trimmed.to_ascii_lowercase());
                }
            }
        }
        report.interfaces = by_name.into_values().collect();
        // Primary IPs: first non-loopback v4/v6.
        report.primary_ipv4 = report
            .interfaces
            .iter()
            .find(|i| !i.loopback && !i.ipv4.is_empty())
            .and_then(|i| i.ipv4.first().cloned());
        report.primary_ipv6 = report
            .interfaces
            .iter()
            .find(|i| !i.loopback && !i.ipv6.is_empty())
            .and_then(|i| i.ipv6.first().cloned());
    }

    // Pod / paired counts straight from the DB. Best-effort: a DB error
    // leaves the fields `None` rather than poisoning the whole snapshot.
    if let Ok(conn) = db::open_default()
        && let Ok(peers) = db::pod::list_peer_summaries(&conn)
    {
        report.pod_peer_count = Some(peers.len() as u32);
        report.pod_paired_count = Some(
            peers
                .iter()
                .filter(|p| p.local_secure && p.peer_secure)
                .count() as u32,
        );
    }
    if let Ok(conn) = db::open_default()
        && let Ok(v) = db::pod::get_self_secure(&conn)
    {
        report.self_secure = Some(v);
    }

    report.mesh_listening = Some(utils::mesh_status::is_listening());
    report.mesh_port = Some(db::ports::mesh_port());

    report
}

/// Detect GPUs — NVIDIA via `nvidia-smi`, AMD via sysfs, Intel via sysfs.
/// Collects from all sources and merges results.
async fn collect_gpus() -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    if let Ok(nvidia) = collect_nvidia_gpus().await {
        gpus.extend(nvidia);
    }
    gpus.extend(collect_amd_gpus());
    gpus.extend(collect_intel_gpus());
    gpus
}

async fn collect_nvidia_gpus() -> anyhow::Result<Vec<GpuInfo>> {
    let out = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await?;
    if !out.status.success() {
        return Ok(vec![]);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut gpus = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.splitn(5, ',').map(str::trim).collect();
        if parts.len() < 5 {
            continue;
        }
        gpus.push(GpuInfo {
            name: parts[0].to_string(),
            vendor: "nvidia".to_string(),
            vram_total_mb: parts[1].parse::<u64>().ok(),
            vram_used_mb: parts[2].parse::<u64>().ok(),
            utilization_percent: parts[3].parse::<f32>().ok(),
            temperature_c: parts[4].parse::<f32>().ok(),
            driver_status: Some("ok".to_string()),
            driver_install_hint: None,
        });
    }
    Ok(gpus)
}

/// Read AMD GPU info from sysfs under `/sys/class/drm/card*/device/`.
fn collect_amd_gpus() -> Vec<GpuInfo> {
    #[cfg(not(target_os = "linux"))]
    return vec![];
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return vec![];
        };
        let mut gpus = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Only top-level card* (not renderD*, card*-*)
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let dev = path.join("device");
            // Must have amdgpu vendor marker
            let vendor_id = std::fs::read_to_string(dev.join("vendor")).unwrap_or_default();
            if !vendor_id.trim().eq_ignore_ascii_case("0x1002") {
                continue;
            }
            let busy: Option<f32> = std::fs::read_to_string(dev.join("gpu_busy_percent"))
                .ok()
                .and_then(|s| s.trim().parse().ok());
            let vram_total: Option<u64> = std::fs::read_to_string(dev.join("mem_info_vram_total"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|b| b / 1024 / 1024);
            let vram_used: Option<u64> = std::fs::read_to_string(dev.join("mem_info_vram_used"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|b| b / 1024 / 1024);
            let card_name = std::fs::read_to_string(dev.join("product_name")).unwrap_or_default();
            let display_name = if card_name.trim().is_empty() {
                format!("AMD GPU ({})", name)
            } else {
                card_name.trim().to_string()
            };
            gpus.push(GpuInfo {
                name: display_name,
                vendor: "amd".to_string(),
                vram_total_mb: vram_total,
                vram_used_mb: vram_used,
                utilization_percent: busy,
                temperature_c: None,
                driver_status: Some("ok".to_string()),
                driver_install_hint: None,
            });
        }
        gpus
    }
}

/// Read Intel GPU info from sysfs under `/sys/class/drm/card*/device/`.
/// iGPUs and Intel Arc discrete GPUs both use vendor `0x8086`. Utilization
/// is not available via stable sysfs — we report the name only.
fn collect_intel_gpus() -> Vec<GpuInfo> {
    #[cfg(not(target_os = "linux"))]
    return vec![];
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return vec![];
        };
        let mut gpus = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let dev = path.join("device");
            let vendor_id = std::fs::read_to_string(dev.join("vendor")).unwrap_or_default();
            if !vendor_id.trim().eq_ignore_ascii_case("0x8086") {
                continue;
            }
            // Intel GPUs may expose gt_act_freq_mhz but not gpu_busy_percent;
            // skip utilization rather than report misleading data.
            let card_name = std::fs::read_to_string(dev.join("product_name")).unwrap_or_default();
            let display_name = if card_name.trim().is_empty() {
                format!("Intel GPU ({})", name)
            } else {
                card_name.trim().to_string()
            };
            gpus.push(GpuInfo {
                name: display_name,
                vendor: "intel".to_string(),
                // iGPUs use shared system RAM; discrete Arc has VRAM but it's
                // not easily readable from stable sysfs without lspci or i915 debugfs.
                vram_total_mb: None,
                vram_used_mb: None,
                utilization_percent: None,
                temperature_c: None,
                // Utilization needs intel-gpu-tools (intel_gpu_top); available
                // via apt install intel-gpu-tools on Debian/Ubuntu.
                driver_status: Some("no_metrics".to_string()),
                driver_install_hint: Some("intel-gpu-tools".to_string()),
            });
        }
        gpus
    }
}

fn orca_dir() -> Option<PathBuf> {
    files::ops::orca_home()
}

/// Returns `(virtualization, dmi_vendor, dmi_product)`.
///
/// Linux: reads `/sys/class/dmi/id/sys_vendor` + `product_name` (KVM/QEMU
/// guests under Proxmox/libvirt show `QEMU` + a generic PC product), plus
/// `/proc/1/cgroup` for container hints. macOS: all `None`.
fn detect_virtualization() -> (Option<String>, Option<String>, Option<String>) {
    #[cfg(not(target_os = "linux"))]
    {
        (None, None, None)
    }
    #[cfg(target_os = "linux")]
    {
        let vendor = std::fs::read_to_string("/sys/class/dmi/id/sys_vendor")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let product = std::fs::read_to_string("/sys/class/dmi/id/product_name")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Container detection first — DMI inside a container reflects the host.
        let cgroup = std::fs::read_to_string("/proc/1/cgroup").unwrap_or_default();
        let virt = if cgroup.contains("/docker/") || cgroup.contains("docker-") {
            Some("docker".to_string())
        } else if cgroup.contains("/lxc/") || cgroup.contains("lxc-") {
            Some("lxc".to_string())
        } else {
            match vendor.as_deref() {
                Some("QEMU") => Some("kvm".to_string()),
                Some("VMware, Inc.") => Some("vmware".to_string()),
                Some("Microsoft Corporation") if product.as_deref() == Some("Virtual Machine") => {
                    Some("hyperv".to_string())
                }
                Some("Xen") => Some("xen".to_string()),
                Some("innotek GmbH") => Some("virtualbox".to_string()),
                Some(_) => Some("none".to_string()),
                None => None,
            }
        };
        (virt, vendor, product)
    }
}

/// Proxmox hosts ship pmxcfs at `/etc/pve/` and the `pveversion` binary —
/// either marker alone is a strong, false-positive-free Proxmox signal that
/// works without root and without shelling out. Guest attribution happens
/// later in the mesh inference layer (tap-MAC match against PVE hosts).
fn detect_proxmox_role() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if std::path::Path::new("/etc/pve").is_dir()
            || std::path::Path::new("/usr/bin/pveversion").is_file()
        {
            return Some("host".to_string());
        }
    }
    None
}

/// Cluster name for a Proxmox host from `/etc/pve/corosync.conf`
/// (`totem { cluster_name: <name> }`). Standalone hosts and non-Proxmox
/// systems have no such file, so the read fails and this is `None`. Read-only,
/// no root, no shelling out; the file is mesh-shared pmxcfs but `cluster_name`
/// is stable per node. Not platform-gated — the path simply won't exist off a
/// Proxmox host, so it degrades to `None` everywhere.
fn detect_pve_cluster() -> Option<String> {
    let content = std::fs::read_to_string("/etc/pve/corosync.conf").ok()?;
    parse_corosync_cluster_name(&content)
}

/// Pull `cluster_name: <name>` out of a corosync.conf body.
fn parse_corosync_cluster_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("cluster_name:") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_corosync_cluster_name() {
        let conf = "totem {\n  version: 2\n  cluster_name: yggdrasil\n  secauth: on\n}\n";
        assert_eq!(
            parse_corosync_cluster_name(conf),
            Some("yggdrasil".to_string())
        );
    }

    #[test]
    fn corosync_without_name_is_none() {
        assert_eq!(
            parse_corosync_cluster_name("totem {\n  version: 2\n}\n"),
            None
        );
    }

    #[test]
    fn collect_blocking_populates_hardware_fields() {
        let snap = collect_blocking();
        assert!(snap.cpu_logical.is_some_and(|c| c > 0));
        assert!(snap.mem_total_mb.is_some_and(|m| m > 0));
        // Integer-divided to MB — under memory pressure this may round to 0;
        // assert presence, not magnitude.
        assert!(snap.mem_available_mb.is_some());
        // mem_used = total - available; must be non-negative
        let total = snap.mem_total_mb.unwrap();
        let used = snap.mem_used_mb.unwrap();
        // Integer division means used may differ from (total - avail) by 1 MiB.
        assert!(
            used <= total + 1,
            "mem_used_mb ({used}) > mem_total_mb ({total})"
        );
    }

    #[test]
    fn collect_blocking_swap_fields_consistent() {
        let snap = collect_blocking();
        let total = snap.swap_total_mb.unwrap_or(0);
        let used = snap.swap_used_mb.unwrap_or(0);
        assert!(
            used <= total,
            "swap_used_mb ({used}) > swap_total_mb ({total})"
        );
    }

    #[test]
    fn collect_blocking_os_fields_present() {
        let snap = collect_blocking();
        assert!(snap.os_name.is_some());
        assert!(snap.snapshot_at_unix.is_some_and(|t| t > 0));
        assert!(snap.arch.is_some());
    }

    #[test]
    fn collect_blocking_cpu_usage_reasonable() {
        // First call on a fresh System: most platforms return 0 (no delta) so
        // the field is None. macOS may return a non-zero value immediately.
        // Either way, when present the value must be in [0, 100].
        let snap = collect_blocking();
        if let Some(pct) = snap.cpu_usage_percent {
            assert!(
                (0.0..=100.0).contains(&pct),
                "cpu_usage_percent out of range: {pct}"
            );
        }
    }

    #[test]
    fn snapshot_from_sys_with_gpus_propagates() {
        let gpu = GpuInfo {
            name: "Test GPU".into(),
            vendor: "test".into(),
            vram_total_mb: Some(8192),
            vram_used_mb: Some(1024),
            utilization_percent: Some(42.0),
            temperature_c: Some(65.0),
            driver_status: Some("ok".into()),
            driver_install_hint: None,
        };
        let sys = System::new_with_specifics(
            RefreshKind::new()
                .with_memory(sysinfo::MemoryRefreshKind::everything())
                .with_cpu(sysinfo::CpuRefreshKind::everything()),
        );
        let snap = snapshot_from_sys(&sys, vec![gpu.clone()]);
        assert_eq!(snap.gpus.len(), 1);
        assert_eq!(snap.gpus[0].name, "Test GPU");
        assert_eq!(snap.gpus[0].vram_total_mb, Some(8192));
    }

    #[test]
    fn which_finds_existing_binary() {
        // Any binary guaranteed to exist on CI and developer machines.
        assert!(which("sh").is_some());
    }

    #[test]
    fn which_returns_none_for_nonexistent() {
        assert!(which("__orca_no_such_binary__").is_none());
    }

    #[test]
    fn rss_exceeds_only_trips_above_limit() {
        let mut report = SystemInfoReport::default();

        // Unknown RSS never trips — no false alarm on a missing reading.
        assert!(!rss_exceeds(&report, 100));

        report.process_rss_mb = Some(100);
        // Equal to the limit is not over it (strict `>`).
        assert!(!rss_exceeds(&report, 100));

        report.process_rss_mb = Some(101);
        assert!(rss_exceeds(&report, 100));
    }
}
