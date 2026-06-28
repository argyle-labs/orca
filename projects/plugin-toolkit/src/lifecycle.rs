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
/// The shared form of every plugin's `now_stamp()` helper.
pub fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
