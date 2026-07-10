//! Async subprocess — a core utility exposed through the toolkit.
//!
//! Plugins spawn processes through this orca-owned surface and never name the
//! runtime's process API. tokio is an internal detail; the types here
//! (`Command`, `Output`, `ExitStatus`) are orca's own, so the executor can be
//! swapped without touching a plugin. Pairs with [`crate::time`]. See
//! [[orca-north-star-abstract-system-differences]] and [[plugins-stay-thin]].

use std::ffi::OsStr;
use std::time::Duration;

/// The outcome of a finished process.
#[derive(Clone, Copy, Debug)]
pub struct ExitStatus {
    /// True if the process exited 0.
    pub success: bool,
    /// The exit code, or `None` if killed by a signal.
    pub code: Option<i32>,
}

/// A finished process's status plus captured output.
#[derive(Clone, Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A subprocess to run. Builder-style; the child is killed on drop so a dropped
/// or timed-out command never leaks a process.
pub struct Command {
    inner: tokio::process::Command,
}

impl Command {
    /// A command that will run `program`.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let mut inner = tokio::process::Command::new(program);
        inner.kill_on_drop(true);
        Self { inner }
    }

    /// Append one argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.inner.arg(arg);
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Run to completion, capturing stdout + stderr.
    pub async fn output(mut self) -> std::io::Result<Output> {
        let out = self.inner.output().await?;
        Ok(Output {
            status: ExitStatus {
                success: out.status.success(),
                code: out.status.code(),
            },
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// Spawn and wait up to `budget`. `Ok(Some(status))` if it exited in time;
    /// `Ok(None)` if it timed out (the child is killed). Output is not captured —
    /// use for liveness/exit-code probes.
    pub async fn status_within(mut self, budget: Duration) -> std::io::Result<Option<ExitStatus>> {
        let mut child = self.inner.spawn()?;
        match crate::time::timeout(budget, child.wait()).await {
            Some(result) => {
                let st = result?;
                Ok(Some(ExitStatus {
                    success: st.success(),
                    code: st.code(),
                }))
            }
            None => {
                // Timed out — request the kill (kill_on_drop reaps it).
                drop(child.start_kill());
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_captures_stdout_and_success() {
        let out = Command::new("printf")
            .arg("hello")
            .output()
            .await
            .expect("run printf");
        assert!(out.status.success);
        assert_eq!(out.stdout, b"hello");
    }

    #[tokio::test]
    async fn status_within_returns_some_for_fast_command() {
        let st = Command::new("true")
            .status_within(Duration::from_secs(5))
            .await
            .expect("run true");
        assert!(matches!(st, Some(ExitStatus { success: true, .. })));
    }

    #[tokio::test]
    async fn status_within_times_out_and_kills() {
        let st = Command::new("sleep")
            .arg("30")
            .status_within(Duration::from_millis(20))
            .await
            .expect("spawn sleep");
        assert!(st.is_none(), "expected timeout → None");
    }
}
