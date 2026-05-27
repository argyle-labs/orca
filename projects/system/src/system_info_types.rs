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

    /// Capabilities the detector observed on this host (e.g. `"docker"`,
    /// `"vm-host"`, `"lxc-host"`, `"backup-target"`, `"gpu-nvidia"`).
    /// Empty when none were detected. Compared against
    /// `expected_capabilities(system_type)` (a static table in the
    /// server crate) to produce anomaly badges in the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_capabilities: Vec<String>,

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
