//! Background-refreshed cross-platform system snapshot powering
//! `system.runtime-spec.system` (and therefore `pod.list[].system`).
//!
//! Collection runs every 30s in a background task spawned at server start.
//! `current()` returns the most recent snapshot — fast (<1µs lock-free read),
//! never blocks on sysinfo. Bootstrapping callers that race the first refresh
//! get `None`; the first snapshot lands ~immediately after `spawn_refresher`.

use orca_tools_def::orca_lifecycle::{NetIfaceDto, SystemInfoReport};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use sysinfo::{Disks, Networks, Pid, ProcessRefreshKind, RefreshKind, System};

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

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
pub fn spawn_refresher() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let snap = tokio::task::spawn_blocking(collect_blocking)
                .await
                .unwrap_or_default();
            if let Ok(mut g) = cache().lock() {
                *g = Some(Arc::new(snap));
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

/// Synchronously collect a fresh snapshot. Used by the refresher and by
/// tests; production callers should use [`current`].
pub fn collect_blocking() -> SystemInfoReport {
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
        ..Default::default()
    };

    // CPU + memory + process. RefreshKind narrows the work so we don't pay
    // for sysinfo's full process scan on every tick.
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_memory(sysinfo::MemoryRefreshKind::everything())
            .with_cpu(sysinfo::CpuRefreshKind::everything())
            .with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_memory();
    sys.refresh_cpu_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    report.cpu_logical = Some(sys.cpus().len() as u32);
    report.cpu_physical = sys.physical_core_count().map(|c| c as u32);
    if let Some(c) = sys.cpus().first() {
        report.cpu_model = Some(c.brand().to_string());
    }
    report.mem_total_mb = Some(sys.total_memory() / 1024 / 1024);
    report.mem_available_mb = Some(sys.available_memory() / 1024 / 1024);
    report.swap_total_mb = Some(sys.total_swap() / 1024 / 1024);

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
    // prefix that contains the orca dir so we report the right volume on
    // hosts with separate /home or /var partitions.
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

fn orca_dir() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(orca_utils::config::APP_STATE_DIR)))
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
