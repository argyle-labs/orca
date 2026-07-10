//! Daemon state file: cooperative port handoff between stable daemon and dev server.
//!
//! `DaemonState` is written to `~/.orca/state.json` (orca's state dir). The dev server reads it to find
//! the daemon's PID, sends SIGUSR1 to park it, then reclaims with SIGUSR2 on exit.
//! All writes go through a tmp-then-rename so readers never see a torn file.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::time::Timestamp;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Current operating mode of the `orca` process that owns the port.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonMode {
    /// Stable background daemon — handles all traffic.
    Daemon,
    /// Parked by SIGUSR1 — port released, waiting for SIGUSR2 to reclaim.
    Parked,
    /// Dev server has the port — hot reload active.
    Dev,
}

/// Persisted runtime state for the orca daemon/dev handoff protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    /// PID of the stable daemon process — persists across park/dev cycles
    pub daemon_pid: u32,
    /// PID of the process currently holding the port (daemon or dev)
    pub active_pid: u32,
    pub port: u16,
    pub mode: DaemonMode,
    /// Path to the installed binary
    pub binary: String,
    pub version: String,
    pub started_at: Timestamp,
}

/// Canonical path for the daemon state file: `<state_dir>/state.json`.
/// Resolves through the canonical path module (honors `$ORCA_HOME`).
pub fn state_path() -> PathBuf {
    contract::config::orca_home()
        .unwrap_or_else(|| PathBuf::from("/tmp").join(".orca"))
        .join("state.json")
}

// ── path-parameterised internals (used by tests and public API alike) ─────────

pub fn read_from(path: &Path) -> Result<Option<DaemonState>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)?;
    let state: DaemonState = serde_json::from_str(&raw)?;
    Ok(Some(state))
}

pub(crate) fn write_to(path: &Path, state: &DaemonState) -> Result<()> {
    let body = serde_json::to_string_pretty(state)?;
    crate::atomic::write_mkdir(path, body.as_bytes())
}

pub(crate) fn clear_at(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub(crate) fn set_mode_at(path: &Path, mode: DaemonMode) -> Result<()> {
    if let Some(mut s) = read_from(path)? {
        s.mode = mode;
        write_to(path, &s)?;
    }
    Ok(())
}

// ── public API (thin wrappers over the path-parameterised internals) ──────────

/// Read the current daemon state, or `None` if the state file does not exist.
pub fn read() -> Result<Option<DaemonState>> {
    read_from(&state_path())
}

/// Persist daemon state to `~/.orca/state.json` — orca's state dir (atomic write).
pub fn write(state: &DaemonState) -> Result<()> {
    write_to(&state_path(), state)
}

/// Update only the `mode` field in the state file, leaving all other fields unchanged.
pub fn set_mode(mode: DaemonMode) -> Result<()> {
    set_mode_at(&state_path(), mode)
}

/// Update only the `active_pid` field in the state file.
pub fn set_active_pid(pid: u32) -> Result<()> {
    if let Some(mut s) = read()? {
        s.active_pid = pid;
        write(&s)?;
    }
    Ok(())
}

/// Remove the state file (called on clean shutdown).
pub fn clear() -> Result<()> {
    clear_at(&state_path())
}

/// Poll state file until mode matches or timeout elapses.
pub async fn wait_for_mode(target: DaemonMode, timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Ok(Some(s)) = read()
            && s.mode == target
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for daemon mode {:?}", target);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Build a minimal valid DaemonState for use in tests.
    fn sample_state() -> DaemonState {
        DaemonState {
            daemon_pid: 1234,
            active_pid: 1234,
            port: 12000,
            mode: DaemonMode::Daemon,
            binary: "/usr/local/bin/orca".to_string(),
            version: "0.1.0".to_string(),
            started_at: crate::time::now(),
        }
    }

    // ── DaemonMode serialization ──────────────────────────────────────────────

    #[test]
    fn daemon_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&DaemonMode::Daemon).unwrap(),
            r#""daemon""#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMode::Parked).unwrap(),
            r#""parked""#
        );
        assert_eq!(serde_json::to_string(&DaemonMode::Dev).unwrap(), r#""dev""#);
    }

    #[test]
    fn daemon_mode_deserializes_lowercase() {
        let mode: DaemonMode = serde_json::from_str(r#""daemon""#).unwrap();
        assert_eq!(mode, DaemonMode::Daemon);

        let mode: DaemonMode = serde_json::from_str(r#""parked""#).unwrap();
        assert_eq!(mode, DaemonMode::Parked);

        let mode: DaemonMode = serde_json::from_str(r#""dev""#).unwrap();
        assert_eq!(mode, DaemonMode::Dev);
    }

    // ── read_from ─────────────────────────────────────────────────────────────

    #[test]
    fn read_from_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        // File does not exist — should be Ok(None), not an error.
        let result = read_from(&path);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn read_from_valid_json_deserializes_correctly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let original = sample_state();
        std::fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        let result = read_from(&path).unwrap().expect("expected Some(state)");
        assert_eq!(result.daemon_pid, 1234);
        assert_eq!(result.port, 12000);
        assert_eq!(result.mode, DaemonMode::Daemon);
        assert_eq!(result.version, "0.1.0");
    }

    #[test]
    fn read_from_malformed_json_returns_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ this is not valid json }").unwrap();

        let result = read_from(&path);
        assert!(result.is_err(), "expected Err on malformed JSON, got Ok");
    }

    // ── write_to + read_from round-trip ───────────────────────────────────────

    #[test]
    fn write_and_read_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let original = sample_state();
        write_to(&path, &original).unwrap();

        let restored = read_from(&path)
            .unwrap()
            .expect("expected Some after write");
        assert_eq!(restored.daemon_pid, original.daemon_pid);
        assert_eq!(restored.active_pid, original.active_pid);
        assert_eq!(restored.port, original.port);
        assert_eq!(restored.mode, original.mode);
        assert_eq!(restored.binary, original.binary);
        assert_eq!(restored.version, original.version);
    }

    #[test]
    fn write_to_creates_parent_directories() {
        let dir = tempdir().unwrap();
        // Nested path that does not yet exist.
        let path = dir.path().join("nested").join("deep").join("state.json");

        let state = sample_state();
        let result = write_to(&path, &state);
        assert!(
            result.is_ok(),
            "write_to should create parent dirs: {:?}",
            result
        );
        assert!(path.exists(), "state file should exist after write_to");
    }

    // ── set_mode_at ───────────────────────────────────────────────────────────

    #[test]
    fn set_mode_at_updates_only_mode_field() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let original = sample_state(); // mode = Daemon
        write_to(&path, &original).unwrap();

        set_mode_at(&path, DaemonMode::Parked).unwrap();

        let updated = read_from(&path)
            .unwrap()
            .expect("expected Some after set_mode_at");
        assert_eq!(updated.mode, DaemonMode::Parked, "mode should be updated");
        // All other fields must be unchanged.
        assert_eq!(updated.daemon_pid, original.daemon_pid);
        assert_eq!(updated.active_pid, original.active_pid);
        assert_eq!(updated.port, original.port);
        assert_eq!(updated.binary, original.binary);
        assert_eq!(updated.version, original.version);
    }

    #[test]
    fn set_mode_at_on_missing_file_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        // No state file — set_mode_at should succeed silently (nothing to update).
        let result = set_mode_at(&path, DaemonMode::Dev);
        assert!(result.is_ok());
        assert!(
            !path.exists(),
            "no file should be created when state is absent"
        );
    }

    // ── clear_at ──────────────────────────────────────────────────────────────

    #[test]
    fn clear_at_removes_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let state = sample_state();
        write_to(&path, &state).unwrap();
        assert!(path.exists());

        clear_at(&path).unwrap();
        assert!(
            !path.exists(),
            "state file should be removed after clear_at"
        );
    }

    #[test]
    fn clear_at_on_missing_file_is_ok() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        // File never existed — should succeed without error.
        let result = clear_at(&path);
        assert!(result.is_ok());
    }

    // ── full lifecycle ─────────────────────────────────────────────────────────

    #[test]
    fn full_lifecycle_write_update_clear() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        // 1. Write initial state.
        let state = sample_state();
        write_to(&path, &state).unwrap();
        assert_eq!(read_from(&path).unwrap().unwrap().mode, DaemonMode::Daemon);

        // 2. Transition through modes.
        set_mode_at(&path, DaemonMode::Parked).unwrap();
        assert_eq!(read_from(&path).unwrap().unwrap().mode, DaemonMode::Parked);

        set_mode_at(&path, DaemonMode::Dev).unwrap();
        assert_eq!(read_from(&path).unwrap().unwrap().mode, DaemonMode::Dev);

        // 3. Clear — file gone.
        clear_at(&path).unwrap();
        assert!(read_from(&path).unwrap().is_none());
    }
}
