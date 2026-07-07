//! Cross-platform OS / hardware / process / network snapshot types.
//!
//! Pure data shapes — no collectors here. Collection logic lives in
//! `server::system_info` (sysinfo-based). Moved out of `fleet::lifecycle`
//! so domain crates can depend on the snapshot shape without pulling in
//! the full install-lifecycle module.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Cross-platform OS / hardware / process / network snapshot. Every field
/// is optional so the same shape works on macOS, Linux, and (eventually)
/// Windows — a collector failure leaves the field `None` rather than
/// breaking the whole report.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct SystemInfoReport {
    // ── OS ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    /// Linux distro long name (`Ubuntu 24.04.2 LTS`). `None` on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,

    // ── Virtualization ──
    /// Hypervisor / container kind: `kvm`, `qemu`, `vmware`, `lxc`,
    /// `docker`, `none`, etc. Linux-only — read from `/sys/class/dmi/id/`
    /// + `/proc/1/cgroup`. macOS reports `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualization: Option<String>,
    /// DMI system vendor (`QEMU`, `Dell Inc.`, `LENOVO`, ...). Linux-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmi_vendor: Option<String>,
    /// DMI product name (`Standard PC (i440FX + PIIX, 1996)`, ...). Linux-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmi_product: Option<String>,
    /// Proxmox role inferred from on-disk markers: `"host"` when
    /// `/etc/pve/` (pmxcfs) is mounted, `"guest"` when the inference
    /// layer matches this VM's MAC to a PVE host's tap interface,
    /// otherwise `None`. NEVER set by user config.
    ///
    /// **Deprecated** — folded into `system_type` (a value of `"proxmox-ve"`
    /// replaces the previous `proxmox_role == "host"` signal). Kept for one
    /// release so older UIs don't blank out; remove after the host-drawer
    /// redesign (Slice 5) ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxmox_role: Option<String>,

    /// Canonical system-type tag for this host. Exactly one value per host.
    /// Drives expected-capability lookup and service-discovery class
    /// selection. Values: `"unraid"`, `"proxmox-ve"`,
    /// `"proxmox-backup-server"`, `"macos"`, `"debian"`, `"alpine"`,
    /// `"nixos"`, `"truenas-scale"`, `"truenas-core"`, `"linux"` (fallback).
    /// `None` only when the detector failed to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_type: Option<String>,

    /// Human-readable label for `system_type` (e.g. `"Proxmox VE"` for
    /// `"proxmox-ve"`). Server-owned so every surface renders identical text
    /// without re-implementing the switch per client. `None` when
    /// `system_type` is also `None`; unknown tags pass through verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_type_label: Option<String>,

    /// Capabilities the detector observed on this host (e.g. `"docker"`,
    /// `"vm-host"`, `"lxc-host"`, `"backup-target"`, `"gpu-nvidia"`).
    /// Empty when none were detected. Compared against
    /// `expected_capabilities(system_type)` (a static table in the
    /// server crate) to produce anomaly badges in the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_capabilities: Vec<String>,

    /// Parallel-indexed human labels for `detected_capabilities`. Same
    /// length and ordering as `detected_capabilities`; empty when that
    /// vector is empty. Server-owned for the same reason as
    /// `system_type_label`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_labels: Vec<String>,

    // ── Hardware ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_logical: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_physical: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// Aggregate CPU utilisation 0–100 %. Requires two sysinfo refreshes;
    /// always `None` on the very first CLI snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usage_percent: Option<f32>,
    /// Memory usage as a percent 0–100. Computed server-side as
    /// `min(100, mem_used_mb / mem_total_mb * 100)`. `None` when either
    /// numerator or denominator is missing, or when `mem_total_mb == 0`.
    /// Numerator: `mem_used_mb` (total − available). Denominator: `mem_total_mb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_percent: Option<f32>,
    /// 1-minute load average normalised against logical CPU count, expressed
    /// as a percent 0–100. Computed server-side as
    /// `min(100, load_avg_1 / cpu_logical * 100)`. `None` when either field
    /// is missing or `cpu_logical == 0` (no load average on Windows).
    /// Numerator: `load_avg_1`. Denominator: `cpu_logical`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_percent: Option<f32>,
    /// Aggregate CPU utilisation 0–100 %, mirroring `cpu_usage_percent`.
    /// Exposed alongside `mem_percent`/`load_percent` so every surface reads
    /// from the same `*_percent` triple instead of mixing field names.
    /// `None` on the first snapshot of a process (sysinfo requires a delta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_available_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_total_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_used_mb: Option<u64>,
    /// GPUs detected on this host (NVIDIA via nvidia-smi; AMD via sysfs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuInfo>,

    // ── Host / uptime ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_uptime_secs: Option<u64>,
    /// Unix load averages (1/5/15 min). `None` on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_avg_1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_avg_5: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_avg_15: Option<f64>,

    // ── This orca process ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_started_at_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_rss_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_threads: Option<u32>,

    /// Top processes by CPU usage at snapshot time. Capped at 10 entries
    /// (drawer renders them as a click-to-histogram table; widening the
    /// list is cheap server-side but wastes wire bytes the UI can't
    /// usefully render). Empty when sysinfo failed to enumerate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_processes: Vec<TopProcess>,

    /// Rolling time-series — last N samples from this host's history ring
    /// (`~/.orca/history/system.jsonl`). Capped at ~720 points (≈1 h at the
    /// 5 s refresh cadence). Empty until the background refresher has
    /// written at least one tick.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<SystemHistoryPoint>,

    // ── Storage (filesystem hosting ~/.orca) ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca_fs_total_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca_fs_avail_gb: Option<u64>,

    // ── Runtime / integrations ──
    /// **Deprecated** — folded into `detected_capabilities` as the `"docker"`
    /// entry. Kept for one release so older UIs don't blank out; remove
    /// after the host-drawer redesign (Slice 5) ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_peer_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_paired_count: Option<u32>,
    /// Tier-2 secrets-storage permission (`self_secure`) for this host.
    /// `true` = this host is authorized to hold encrypted secrets replicated
    /// from other pod members. Surfaced in the host drawer as a SECURE
    /// toggle, independent of cert trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_secure: Option<bool>,

    /// `true` when this host's mesh accept loop (plugin host, default
    /// port 12002) currently holds a TCP listener. `false` when the
    /// bind failed at startup, the host was stopped, or it has not yet
    /// started. Surfaces silent mesh-port bind failures that previously
    /// rendered a host as "healthy" in `system_detail` while it was
    /// invisible to peers. See [[project-system-detail-hides-mesh-bind-failure]].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_listening: Option<bool>,
    /// Port the mesh accept loop is configured to bind. Paired with
    /// `mesh_listening` so operators can see *which* port is (or isn't)
    /// open without cross-referencing config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_port: Option<u16>,

    // ── Network interfaces ──
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<NetIfaceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ipv6: Option<String>,

    /// Wall-clock when this snapshot was collected. Cached snapshots may be
    /// up to ~30s stale; consumers use this to decide whether to trust a
    /// metric like load average.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_at_unix: Option<i64>,

    // ── Topology ──
    /// Inferred parent in the physical/virtual/container hierarchy.
    /// Set by the inference task (mac-match on peers' `claims`), never by
    /// user config. `None` until a claim matches one of this host's
    /// interface MACs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_peer_id: Option<String>,
    /// Kind of parent edge: `"hypervisor"` (VM under a host), `"host"`
    /// (container under its docker host), or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    /// Things this host claims to host (VMs it runs, containers under its
    /// docker socket, LXCs, etc.). Populated by colocated provider plugins
    /// (proxmox, unraid, docker, ...). Each entry's `macs` is the join key
    /// the inference layer matches against other peers' `interfaces[].mac`.
    /// `TopologyClaim` lives in `contract` so plugins can emit it without
    /// depending on `system`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<contract::TopologyClaim>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct NetIfaceDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ipv4: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ipv6: Vec<String>,
    /// True for loopback interfaces (lo / lo0).
    #[serde(default)]
    pub loopback: bool,
}

/// One process in the host's top-N-by-CPU snapshot. Names are basenames
/// (e.g. `plex-media-server`), not full argv. Memory is RSS in MiB.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct TopProcess {
    pub pid: u32,
    pub name: String,
    /// Aggregate CPU% across all cores (0..=100*N_cores in sysinfo's units;
    /// the collector normalises to a 0..=100 single-core scale before
    /// emitting).
    pub cpu_percent: f32,
    /// Resident set size in MiB.
    pub mem_mb: u64,
}

/// One sample in the per-host rolling history ring. Written every refresh
/// tick by the daemon, read back as `SystemInfoReport.history`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct SystemHistoryPoint {
    /// Unix seconds at sample time.
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_mb: Option<u64>,
    /// RSS of this orca process at sample time, in MiB. Lets the history
    /// ring carry the daemon's own memory footprint alongside host memory
    /// so a leak in orca is distinguishable from host-wide pressure.
    #[serde(default)]
    pub process_rss_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuPoint>,
}

/// One GPU's reading inside a `SystemHistoryPoint`. Matched to a live
/// `GpuInfo` by `name` (driver-stable across ticks).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct GpuPoint {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization_percent: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_used_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_total_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
}

/// One GPU detected on the host.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct GpuInfo {
    /// Display name from driver (e.g. `NVIDIA GeForce RTX 4090`).
    pub name: String,
    /// Source driver: `"nvidia"`, `"amd"`, `"intel"`.
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_total_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_used_mb: Option<u64>,
    /// GPU core utilisation 0–100 %.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization_percent: Option<f32>,
    /// GPU temperature in °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
    /// Driver/tool availability: `"ok"` when metrics are live, `"no_driver"`
    /// when the GPU was detected via sysfs/PCI but the user-space driver or
    /// query tool is absent, `"no_metrics"` when the driver is loaded but
    /// doesn't expose utilization (e.g. Intel iGPU without `intel_gpu_top`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_status: Option<String>,
    /// Suggested package to install to get full metrics. Distro-specific;
    /// only populated when `driver_status = "no_driver"` or `"no_metrics"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_install_hint: Option<String>,
}
