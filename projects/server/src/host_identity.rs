//! Stable per-machine identity.
//!
//! Two facts about a host that callers in the pod-mesh code need:
//!
//!   * **`hostname()`** — a *display* label for humans. macOS rewrites the
//!     OS hostname on mDNS conflicts (`mint` → `mint-2` → `mint-10`), and
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
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static HOSTNAME: OnceLock<String> = OnceLock::new();
static MACHINE_ID: OnceLock<String> = OnceLock::new();

/// Capture the hostname once and load (or generate) the persistent
/// machine_id. Safe to call more than once; subsequent calls are no-ops.
pub fn init(app_dir: &Path) -> Result<()> {
    let hostname = capture_hostname();
    let _ = HOSTNAME.set(hostname);

    let machine_id = load_or_generate_machine_id(app_dir).context("load or generate machine_id")?;
    let _ = MACHINE_ID.set(machine_id);
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

/// Stable per-machine UUID. Panics if `init` has not run.
pub fn machine_id() -> &'static str {
    MACHINE_ID
        .get()
        .expect("host_identity::init() must run before machine_id()")
        .as_str()
}

/// First 12 chars of the machine_id, useful where a short label is
/// needed (cert CNs, peer_id prefixes).
pub fn machine_id_short() -> &'static str {
    &machine_id()[..12]
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
    std::fs::create_dir_all(app_dir).with_context(|| format!("create {}", app_dir.display()))?;
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, format!("{id}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_macos_numeric_suffix() {
        assert_eq!(strip_macos_suffix("mint-2"), "mint");
        assert_eq!(strip_macos_suffix("mint-10"), "mint");
        assert_eq!(strip_macos_suffix("mint"), "mint");
        assert_eq!(strip_macos_suffix("mint.local"), "mint.local");
        // -alpha is not a conflict suffix
        assert_eq!(strip_macos_suffix("mint-alpha"), "mint-alpha");
        // Hostname with legitimate hyphens but no trailing digits
        assert_eq!(strip_macos_suffix("home-server"), "home-server");
    }

    #[test]
    fn machine_id_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_generate_machine_id(dir.path()).unwrap();
        let b = load_or_generate_machine_id(dir.path()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 36); // uuid v4 hyphenated
    }
}
