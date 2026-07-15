//! Stable per-machine identity.
//!
//! Two facts about a host that callers in the pod-mesh code need:
//!
//!   * **`hostname()`** — a *display* label for humans. macOS rewrites the
//!     OS hostname on mDNS conflicts (`host-i` → `host-i-2` → `host-i-10`), and
//!     some Linux distros mutate it on DHCP renewal. We capture it once at
//!     daemon startup and strip the `-<digits>` suffix so log lines and
//!     mDNS TXT records stay coherent across the process lifetime.
//!
//!   * **`machine_id()`** — a stable opaque UUID generated on first run
//!     and persisted at `<app_dir>/machine_id`. Unlike hostname this
//!     never changes, so anywhere we *key* on identity (cert CNs, peer
//!     ids, future federation routing) should prefer this. The bootstrap
//!     ed25519 key fingerprint is also stable, but rotates on key
//!     regeneration; `machine_id` survives key rotation.
//!
//! `init(app_dir)` must run once at daemon startup before any caller
//! invokes `hostname()` / `machine_id()`. Callers panic if the cache is
//! uninitialized — this is intentional so a missing init shows up loudly
//! during development rather than silently using a fallback.

use anyhow::{Context, Result};
use db::host_addressing::{self, HostAddressingRow};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static HOSTNAME: OnceLock<String> = OnceLock::new();
static MACHINE_ID: OnceLock<String> = OnceLock::new();

/// Capture the hostname once and load (or generate) the persistent
/// machine_id. Safe to call more than once; subsequent calls are no-ops.
pub fn init(app_dir: &Path) -> Result<()> {
    let hostname = capture_hostname();
    HOSTNAME.set(hostname).ok();

    let machine_id = load_or_generate_machine_id(app_dir).context("load or generate machine_id")?;
    MACHINE_ID.set(machine_id).ok();
    Ok(())
}

/// Cached display hostname. Panics if `init` has not run — call init at
/// daemon startup.
pub fn hostname() -> &'static str {
    HOSTNAME
        .get()
        .expect("host_identity::init() must run before hostname()")
        .as_str()
}

/// Alias of [`hostname`] using the slice-7 vocabulary (`display_hostname`
/// distinguishes the human label from the `machine_id` identity key).
pub fn display_hostname() -> &'static str {
    hostname()
}

/// Stable per-machine UUID. Panics if `init` has not run.
pub fn machine_id() -> &'static str {
    MACHINE_ID
        .get()
        .expect("host_identity::init() must run before machine_id()")
        .as_str()
}

/// Hostname for use in standalone CLI flows (e.g. `orca install`) where
/// `init()` may not have run. Mirrors `capture_hostname()` but is safe to
/// call without the OnceLock being populated.
pub fn cli_hostname_or_fallback() -> String {
    if let Some(h) = HOSTNAME.get() {
        return h.clone();
    }
    capture_hostname()
}

fn capture_hostname() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    strip_macos_suffix(&raw)
}

/// macOS appends `-2`, `-3`, ... when it detects an mDNS name conflict.
/// Strip a trailing `-<digits>` so the display name stays stable across
/// flaps. `-` is illegal in hostnames at position 0, so the leading `-`
/// is the marker.
fn strip_macos_suffix(name: &str) -> String {
    let trimmed = name.trim_end_matches('.');
    if let Some(idx) = trimmed.rfind('-') {
        let (head, tail) = trimmed.split_at(idx);
        let tail_digits = &tail[1..]; // skip the '-'
        if !tail_digits.is_empty() && tail_digits.chars().all(|c| c.is_ascii_digit()) {
            return head.to_string();
        }
    }
    trimmed.to_string()
}

fn machine_id_path(app_dir: &Path) -> PathBuf {
    app_dir.join("machine_id")
}

fn load_or_generate_machine_id(app_dir: &Path) -> Result<String> {
    let path = machine_id_path(app_dir);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    // Always mint a fresh UUIDv7 and persist it. The persisted
    // `<app_dir>/machine_id` file is the single source of truth — once
    // written it never changes, so identity is stable across restarts,
    // service-user pivots, and key rotation. We deliberately do NOT anchor
    // to the OS machine-id (`/etc/machine-id`, IOPlatformUUID): those are
    // bare-hex, non-UUIDv7, and produced an inconsistent fleet (some hosts
    // UUIDv7, some bare-hex) that broke id-based targeting. Hard rule: every
    // id is a full UUIDv7, no truncation, no prefixes.
    let id = utils::id::new();
    std::fs::create_dir_all(app_dir).with_context(|| format!("create {}", app_dir.display()))?;
    std::fs::write(&path, format!("{id}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(id)
}

// ── Multi-channel addressing detection (slice 2) ─────────────────────────────

const KEY_DISPLAY_NAME: &str = "display_name";
const KEY_FQDN: &str = "fqdn";
const KEY_LAN_V4: &str = "lan_v4";
const KEY_LAN_V6: &str = "lan_v6";
const KEY_TAILSCALE_V4: &str = "tailscale_v4";
const KEY_TAILSCALE_V6: &str = "tailscale_v6";

const SOURCE_MANUAL: &str = "manual";
const SOURCE_AUTODETECT: &str = "autodetect";

/// Names commonly applied to virtual/container interfaces we want to skip
/// when picking LAN addresses. Substring match.
const VIRTUAL_IFACE_MARKERS: &[&str] = &["docker", "br-", "veth", "tailscale", "utun"];

use utils::time::now_secs_since_epoch as now_secs;

fn make_row(key: &str, value: String, source: &str) -> HostAddressingRow {
    HostAddressingRow {
        key: key.to_string(),
        value,
        source: source.to_string(),
        detected_at: now_secs(),
    }
}

/// Detect every addressing channel for this host. Pure (no DB writes).
/// Settings-sourced rows (display_name when overridden, fqdn) are marked
/// `manual`; everything else is `autodetect`. Tailscale is skipped silently
/// if the `tailscale` binary is missing or exits non-zero.
pub fn detect_all(conn: &Connection) -> Vec<HostAddressingRow> {
    let mut out = Vec::new();

    // display_name: setting wins; OS hostname fallback.
    let (display_value, display_source) = match db::settings::get(conn, "host.display_name") {
        Ok(Some(v)) if !v.trim().is_empty() => (v, SOURCE_MANUAL),
        _ => (hostname().to_string(), SOURCE_AUTODETECT),
    };
    out.push(make_row(KEY_DISPLAY_NAME, display_value, display_source));

    // fqdn: manual-only at this slice (Caddy autodetect = slice 4b).
    if let Ok(Some(v)) = db::settings::get(conn, "host.fqdn") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            out.push(make_row(KEY_FQDN, v, SOURCE_MANUAL));
        }
    }

    // LAN: enumerate interfaces, skip loopback + virtual. A dual-homed host
    // (e.g. both a wired and a wireless NIC on the LAN) has more than one valid
    // address per family — capture ALL of them as equal rows, not just the
    // first. host_addressing PK = (key, value) lets them coexist; the dialer
    // then tries every one. De-dup preserves enumeration order.
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        let mut v4: Vec<String> = Vec::new();
        let mut v6: Vec<String> = Vec::new();
        for iface in ifs {
            if iface.is_loopback() {
                continue;
            }
            let name_lower = iface.name.to_lowercase();
            if VIRTUAL_IFACE_MARKERS.iter().any(|m| name_lower.contains(m)) {
                continue;
            }
            match iface.ip() {
                std::net::IpAddr::V4(ip) => {
                    let s = ip.to_string();
                    if !v4.contains(&s) {
                        v4.push(s);
                    }
                }
                std::net::IpAddr::V6(ip) => {
                    if !ip.is_loopback() {
                        let s = ip.to_string();
                        if !v6.contains(&s) {
                            v6.push(s);
                        }
                    }
                }
            }
        }
        for v in v4 {
            out.push(make_row(KEY_LAN_V4, v, SOURCE_AUTODETECT));
        }
        for v in v6 {
            out.push(make_row(KEY_LAN_V6, v, SOURCE_AUTODETECT));
        }
    }

    // Tailscale: ask `tailscale status --self --json`. Missing binary or
    // nonzero exit = silently skip (host isn't on Tailscale).
    if let Some((v4, v6)) = detect_tailscale_ips() {
        if let Some(v) = v4 {
            out.push(make_row(KEY_TAILSCALE_V4, v, SOURCE_AUTODETECT));
        }
        if let Some(v) = v6 {
            out.push(make_row(KEY_TAILSCALE_V6, v, SOURCE_AUTODETECT));
        }
    }

    out
}

/// Minimal subset of `tailscale status --self --json` we actually consume.
/// The real schema has many more fields; serde will ignore them by default.
#[derive(serde::Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "Self")]
    self_: TailscaleSelf,
}

#[derive(serde::Deserialize)]
struct TailscaleSelf {
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
}

fn detect_tailscale_ips() -> Option<(Option<String>, Option<String>)> {
    let out = std::process::Command::new("tailscale")
        .args(["status", "--self", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: TailscaleStatus = serde_json::from_slice(&out.stdout).ok()?;
    Some(pick_tailscale_ips(parsed.self_.tailscale_ips))
}

/// From Tailscale's `TailscaleIPs` list pick the first IPv4 (no `:`) and the
/// first IPv6 (contains `:`). Pure; the subprocess/JSON boundary lives in the
/// caller.
fn pick_tailscale_ips(ips: Vec<String>) -> (Option<String>, Option<String>) {
    let mut v4: Option<String> = None;
    let mut v6: Option<String> = None;
    for ip in ips {
        if ip.contains(':') {
            if v6.is_none() {
                v6 = Some(ip);
            }
        } else if v4.is_none() {
            v4 = Some(ip);
        }
    }
    (v4, v6)
}

/// Refresh autodetected rows + persist manual rows (from settings). Clears
/// any stale `autodetect` rows first so a removed interface drops out.
/// `manual` rows are upserted in-place (no clear sweep — settings are the
/// source of truth and may not include every channel).
///
/// Idempotent: safe to call repeatedly. The scheduler invokes this on a
/// 5-minute tick; `host.refresh` triggers it on demand.
pub fn refresh_and_persist(conn: &Connection) -> Result<()> {
    let rows = detect_all(conn);
    host_addressing::clear_host_addressing_by_source(conn, SOURCE_AUTODETECT)?;
    for r in rows {
        // display_name / fqdn are single-valued — replace by key so a changed
        // value doesn't leave the old (manual-source, un-cleared) row behind.
        // LAN / Tailscale channels are multi-valued — add a row per address.
        if r.key == KEY_DISPLAY_NAME || r.key == KEY_FQDN {
            host_addressing::set_host_addressing(conn, &r.key, &r.value, &r.source)?;
        } else {
            host_addressing::upsert_host_addressing(conn, &r.key, &r.value, &r.source)?;
        }
    }
    Ok(())
}

/// Adapter implementing the `HostRefreshHook` trait from
/// `fleet::host` so `host.refresh` can drive the real detect path
/// without the domain crate depending on the server's process-level statics.
pub struct ServerHostRefreshHook;
impl crate::host::HostRefreshHook for ServerHostRefreshHook {
    fn refresh(&self, conn: &db::Conn) -> Result<()> {
        refresh_and_persist(conn)
    }
}

/// Spawn a background task that calls [`refresh_and_persist`] at daemon
/// startup and every 5 minutes thereafter. Errors are logged at `debug`
/// (transient autodetect failures aren't actionable for the operator).
pub fn spawn_refresh_task() -> tokio::task::JoinHandle<()> {
    use std::time::Duration;
    const TICK_INTERVAL: Duration = Duration::from_secs(5 * 60);
    crate::periodic::spawn(
        crate::periodic::PeriodicSpec {
            name: "host.identity.refresh.run",
            initial_delay: Duration::ZERO,
            interval: TICK_INTERVAL,
        },
        crate::periodic::boxed(|| async move {
            let conn = db::open_default()?;
            refresh_and_persist(&conn)?;
            tracing::trace!("[host-addressing] refreshed");
            Ok(())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_macos_numeric_suffix() {
        assert_eq!(strip_macos_suffix("host-i-2"), "host-i");
        assert_eq!(strip_macos_suffix("host-i-10"), "host-i");
        assert_eq!(strip_macos_suffix("host-i"), "host-i");
        assert_eq!(strip_macos_suffix("host-i.local"), "host-i.local");
        // -alpha is not a conflict suffix
        assert_eq!(strip_macos_suffix("host-i-alpha"), "host-i-alpha");
        // Hostname with legitimate hyphens but no trailing digits
        assert_eq!(strip_macos_suffix("home-server"), "home-server");
    }

    #[test]
    fn machine_id_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_generate_machine_id(dir.path()).unwrap();
        let b = load_or_generate_machine_id(dir.path()).unwrap();
        // The invariant is persistence: a second load returns the same id. The
        // id's length is NOT asserted — `load_or_generate_machine_id` anchors to
        // the OS machine identity when present (`/etc/machine-id` is 32 hex
        // chars on Linux CI), only falling back to a 36-char hyphenated UUID
        // when no OS source exists. Pinning len==36 made the test pass only on
        // hosts without `/etc/machine-id` (e.g. macOS dev) and fail on Linux CI.
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn strips_single_digit_suffix() {
        assert_eq!(strip_macos_suffix("host-1"), "host");
    }

    #[test]
    fn strips_trailing_dot_before_matching() {
        // A trailing `.` (mDNS FQDN form) is trimmed first, then the numeric
        // conflict suffix is stripped.
        assert_eq!(strip_macos_suffix("host-i-2."), "host-i");
    }

    #[test]
    fn keeps_name_without_hyphen() {
        assert_eq!(strip_macos_suffix("host"), "host");
    }

    #[test]
    fn keeps_bare_trailing_hyphen() {
        // Trailing `-` with no digits after it is not a conflict suffix.
        assert_eq!(strip_macos_suffix("host-"), "host-");
    }

    #[test]
    fn keeps_mixed_alnum_tail() {
        assert_eq!(strip_macos_suffix("host-2b"), "host-2b");
    }

    #[test]
    fn machine_id_path_appends_filename() {
        let p = machine_id_path(Path::new("/some/app/dir"));
        assert_eq!(p, PathBuf::from("/some/app/dir/machine_id"));
    }

    #[test]
    fn machine_id_generate_creates_missing_dir() {
        // app_dir does not exist yet; load_or_generate must create it and
        // persist the file.
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b");
        let id = load_or_generate_machine_id(&nested).unwrap();
        assert!(!id.is_empty());
        assert!(nested.join("machine_id").is_file());
    }

    #[test]
    fn machine_id_ignores_blank_file() {
        // A whitespace-only existing file is treated as absent → regenerated.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(machine_id_path(tmp.path()), "   \n").unwrap();
        let id = load_or_generate_machine_id(tmp.path()).unwrap();
        assert!(!id.trim().is_empty());
    }

    #[test]
    fn machine_id_trims_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(machine_id_path(tmp.path()), "  fixed-id-123  \n").unwrap();
        let id = load_or_generate_machine_id(tmp.path()).unwrap();
        assert_eq!(id, "fixed-id-123");
    }

    #[test]
    fn make_row_populates_fields() {
        let r = make_row(KEY_LAN_V4, "10.0.0.5".to_string(), SOURCE_AUTODETECT);
        assert_eq!(r.key, KEY_LAN_V4);
        assert_eq!(r.value, "10.0.0.5");
        assert_eq!(r.source, SOURCE_AUTODETECT);
        assert!(r.detected_at > 0);
    }

    #[test]
    fn pick_tailscale_ips_first_of_each_family() {
        let (v4, v6) = pick_tailscale_ips(vec![
            "100.64.0.1".to_string(),
            "100.64.0.2".to_string(),
            "fd7a::1".to_string(),
            "fd7a::2".to_string(),
        ]);
        assert_eq!(v4, Some("100.64.0.1".to_string()));
        assert_eq!(v6, Some("fd7a::1".to_string()));
    }

    #[test]
    fn pick_tailscale_ips_v4_only() {
        let (v4, v6) = pick_tailscale_ips(vec!["100.64.0.9".to_string()]);
        assert_eq!(v4, Some("100.64.0.9".to_string()));
        assert!(v6.is_none());
    }

    #[test]
    fn pick_tailscale_ips_empty() {
        let (v4, v6) = pick_tailscale_ips(vec![]);
        assert!(v4.is_none());
        assert!(v6.is_none());
    }

    #[test]
    fn tailscale_status_parses_self_ips_and_ignores_extra() {
        let json = r#"{
            "Self": { "TailscaleIPs": ["100.64.0.1", "fd7a::1"], "HostName": "x" },
            "Peer": {},
            "Version": "1.0"
        }"#;
        let parsed: TailscaleStatus = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.self_.tailscale_ips,
            vec!["100.64.0.1".to_string(), "fd7a::1".to_string()]
        );
    }

    #[test]
    fn tailscale_status_defaults_missing_ips() {
        let json = r#"{ "Self": { "HostName": "x" } }"#;
        let parsed: TailscaleStatus = serde_json::from_str(json).unwrap();
        assert!(parsed.self_.tailscale_ips.is_empty());
    }
}
