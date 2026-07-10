//! Shared helpers for plugin deploy-lifecycle tool surfaces (`*.install` /
//! `*.update` / `*.backup` / `*.restore`).
//!
//! Composition, not inheritance: these are small free functions a lifecycle
//! module calls, not a trait or base type it implements. They capture the
//! exec/stderr/timestamp boilerplate that was byte-identical across every
//! deploy-engine plugin (docker, dockge, jellyfin, homeassistant, plex, ntfy),
//! so the per-plugin `lifecycle.rs` keeps only the commands unique to that
//! service.
//!
//! Process exec crosses raw OS output here; `disallowed_types` is irrelevant
//! (no JSON), but the module is light-core — no feature gate.

use std::process::Output;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Run a command to completion, capturing output, and map a non-zero exit to an
/// error that carries `stderr` — lifecycle tools surface the runtime's own
/// message rather than a bare exit code. Returns the full [`Output`] on success
/// so callers can read `stdout` (e.g. via [`stdout_string`]).
///
/// This is the exact helper that was hand-copied into six plugins' lifecycle
/// modules; adopting it makes each of those a pure deletion.
pub async fn run(cmd: &mut Command) -> Result<Output> {
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to spawn command".to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed ({}): {}", output.status, stderr.trim());
    }
    Ok(output)
}

/// Decode a command's captured `stdout` as a lossy UTF-8 `String` — the `log`
/// field every lifecycle tool returns. Pairs with [`run`].
pub fn stdout_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A sortable UTC timestamp (`YYYYMMDD-HHMMSS`) for naming backup artifacts.
/// The shared form of every plugin's `now_stamp()` helper. Computed from the
/// system clock with no datetime-library dependency (the toolkit's always-on
/// core links no chrono); code needing a full instant uses
/// [`crate::time::Timestamp`].
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Decompose Unix seconds (UTC) into `(year, month, day, hour, min, sec)` using
/// Howard Hinnant's `civil_from_days` algorithm — pure integer math, no deps.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_unix_matches_known_instants() {
        // 2026-07-09T18:20:05Z
        assert_eq!(civil_from_unix(1_783_621_205), (2026, 7, 9, 18, 20, 5));
        // Epoch.
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // A leap day: 2024-02-29T23:59:59Z.
        assert_eq!(civil_from_unix(1_709_251_199), (2024, 2, 29, 23, 59, 59));
    }

    #[tokio::test]
    async fn run_returns_stdout_on_success() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello-orca");
        let out = run(&mut cmd).await.expect("echo succeeds");
        assert_eq!(stdout_string(&out).trim(), "hello-orca");
    }

    #[tokio::test]
    async fn run_surfaces_stderr_on_failure() {
        // `false` exits non-zero with no output; assert we map it to an error
        // rather than returning a failed Output.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo boom >&2; exit 3"]);
        let err = run(&mut cmd).await.expect_err("non-zero exit is an error");
        let msg = format!("{err}");
        assert!(msg.contains("command failed"), "got: {msg}");
        assert!(msg.contains("boom"), "stderr not surfaced: {msg}");
    }

    #[test]
    fn timestamp_is_sortable_shape() {
        let t = timestamp();
        assert_eq!(t.len(), "YYYYMMDD-HHMMSS".len());
        let (date, time) = t.split_once('-').expect("has a dash");
        assert!(date.chars().all(|c| c.is_ascii_digit()));
        assert!(time.chars().all(|c| c.is_ascii_digit()));
    }
}
