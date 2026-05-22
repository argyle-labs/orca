//! Background-refreshed cross-platform system snapshot powering
//! `system.runtime-spec.system` (and therefore `pod.list[].system`).
//!
//! Collection runs every 30s in a background task spawned at server start.
//! `current()` returns the most recent snapshot — fast (<1µs lock-free read),
//! never blocks on sysinfo. Bootstrapping callers that race the first refresh
//! get `None`; the first snapshot lands ~immediately after `spawn_refresher`.

use orca_tools_def::orca_lifecycle::{GpuInfo, NetIfaceDto, SystemInfoReport};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use sysinfo::{Disks, Networks, Pid, ProcessRefreshKind, RefreshKind, System};

/// In-memory cache refresh interval. Short so any client poll (UI every
/// ~1-10s, MCP, CLI) gets near-live data without re-running sysinfo on every
/// call. DB persistence runs on its own slower cadence — see
/// `crate::host_status_writer`.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

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

        loop {
            tokio::time::sleep(REFRESH_INTERVAL).await;
            sys.refresh_memory();
            sys.refresh_cpu_all();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

            let gpus = collect_gpus().await;
            let snap = Arc::new(snapshot_from_sys(&sys, gpus));
            if let Ok(mut g) = cache().lock() {
                *g = Some(snap);
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
        && let Ok(peers) = db::pod::list_peers(&conn)
    {
        report.pod_peer_count = Some(peers.len() as u32);
        report.pod_paired_count = Some(
            peers
                .iter()
                .filter(|p| p.local_secure && p.peer_secure)
                .count() as u32,
        );
    }

    report
}

/// Detect GPUs — NVIDIA via `nvidia-smi`, AMD via sysfs.
/// Returns empty vec if no GPUs or driver absent.
async fn collect_gpus() -> Vec<GpuInfo> {
    // Try NVIDIA first (most common in homelab GPU hosts).
    if let Ok(gpus) = collect_nvidia_gpus().await
        && !gpus.is_empty()
    {
        return gpus;
    }
    // Fallback: AMD sysfs
    collect_amd_gpus()
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
            });
        }
        gpus
    }
}

fn orca_dir() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(orca_utils::config::APP_STATE_DIR)))
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
    fn collect_blocking_populates_hardware_fields() {
        let snap = collect_blocking();
        assert!(snap.cpu_logical.is_some_and(|c| c > 0));
        assert!(snap.mem_total_mb.is_some_and(|m| m > 0));
        assert!(snap.mem_available_mb.is_some_and(|m| m > 0));
        // mem_used = total - available; must be non-negative
        let total = snap.mem_total_mb.unwrap();
        let used = snap.mem_used_mb.unwrap();
        let avail = snap.mem_available_mb.unwrap();
        assert!(
            used <= total,
            "mem_used_mb ({used}) > mem_total_mb ({total})"
        );
        assert_eq!(used, total - avail, "mem_used_mb mismatch");
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
    fn collect_blocking_cpu_usage_absent_on_first_call() {
        // First call creates a fresh System — no delta, so usage should be None.
        let snap = collect_blocking();
        // cpu_usage_percent is None OR 0 on first call (no prior state).
        let ok =
            snap.cpu_usage_percent.is_none() || snap.cpu_usage_percent.is_some_and(|u| u == 0.0);
        assert!(
            ok,
            "expected None or 0 on first call, got {:?}",
            snap.cpu_usage_percent
        );
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
}
