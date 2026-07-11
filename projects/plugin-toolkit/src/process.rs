//! Async subprocess — a core utility exposed through the toolkit.
//!
//! Plugins spawn processes through this orca-owned surface and never name the
//! runtime's process API. tokio is an internal detail; the types here
//! (`Command`, `Output`, `ExitStatus`) are orca's own, so the executor can be
//! swapped without touching a plugin. Pairs with [`crate::time`]. See
//! [[orca-north-star-abstract-system-differences]] and [[plugins-stay-thin]].
//
// JSON-RPC request/response lines are a genuinely free-form transport-dynamic
// boundary — an id is injected and responses are correlated by it, but the
// message bodies are the peer's own schema, not ours. `serde_json::Value` is the
// sanctioned escape hatch here, scoped to this seam. See [`Child::request`].
#![allow(clippy::disallowed_types)]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

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

    /// Set an environment variable for the child. Lets a plugin inject e.g.
    /// `DOCKER_HOST` without naming the executor's process API.
    pub fn env(mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> Self {
        self.inner.env(key, val);
        self
    }

    /// Set the child's working directory.
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.inner.current_dir(dir);
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

    /// Spawn a long-lived child with piped stdin/stdout, returning an orca-owned
    /// [`Child`] handle. The analogue of [`Command::output`] / [`Command::status_within`]
    /// for processes a plugin talks to over their lifetime — e.g. a JSON-RPC peer
    /// spoken to line-by-line — rather than running to completion in one shot.
    ///
    /// stderr is inherited from the parent so the child's diagnostics surface in
    /// the operator's logs. Like every seam here the child is killed on drop, so a
    /// dropped [`Child`] never leaks the subprocess. The plugin never names the
    /// runtime's process, stdin, or stdout types — they are orca's own.
    pub fn spawn(mut self) -> std::io::Result<Child> {
        self.inner
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = self.inner.spawn()?;
        let stdin = child
            .stdin
            .take()
            .expect("stdin was piped, so it is present");
        let stdout = child
            .stdout
            .take()
            .expect("stdout was piped, so it is present");
        Ok(Child {
            inner: child,
            stdio: Mutex::new(Stdio {
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(1),
        })
    }
}

/// The child's pipe handles, held together behind [`Child`]'s internal
/// [`Mutex`] so a single lock serializes a whole write→read exchange — no two
/// concurrent callers can interleave a request and steal each other's response.
struct Stdio {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// A running long-lived subprocess with line-oriented stdin/stdout, spawned via
/// [`Command::spawn`]. Owns the child and its pipe handles; killed on drop so a
/// dropped `Child` never leaks the process.
///
/// The wrapped tokio [`Child`](tokio::process::Child) / [`ChildStdin`] /
/// `BufReader<ChildStdout>` are internal — a plugin drives the peer through the
/// request/response API ([`request`](Child::request) / [`notify`](Child::notify))
/// or the low-level [`write_line`](Child::write_line) /
/// [`read_line`](Child::read_line), and never names the runtime's process API.
///
/// The stdio handles live behind an internal async [`Mutex`], so every method
/// takes `&self`: concurrent callers are serialized in-core and cannot corrupt
/// or interleave each other's lines. A JSON-RPC `id` is minted per
/// [`request`](Child::request) from an in-core atomic counter and responses are
/// correlated by that id, so many concurrent requests never cross-talk.
pub struct Child {
    inner: tokio::process::Child,
    stdio: Mutex<Stdio>,
    next_id: AtomicU64,
}

impl Child {
    /// Send a JSON-RPC request and await its correlated response, up to `timeout`.
    ///
    /// `line` is parsed as a JSON object; a fresh in-core `id` (an atomic
    /// counter) is injected, overwriting any caller-supplied `id`. The request is
    /// written and responses are read back under the single stdio lock, and only
    /// the line whose `"id"` matches the one we minted is returned — any
    /// non-matching line (a stray notification, or a response to another peer
    /// message) is skipped. Because the whole write→correlate exchange holds the
    /// lock, concurrent `request` calls are serialized and can never receive each
    /// other's response. Returns the raw matching response line (no trailing
    /// newline).
    pub async fn request(&self, line: &str, timeout: Duration) -> Result<String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg: serde_json::Value =
            serde_json::from_str(line).context("request line is not valid JSON")?;
        let obj = msg
            .as_object_mut()
            .ok_or_else(|| anyhow!("request line is not a JSON object"))?;
        obj.insert("id".to_string(), serde_json::Value::from(id));
        let payload = serde_json::to_string(&msg)?;

        let fut = async {
            let mut stdio = self.stdio.lock().await;
            stdio.stdin.write_all(payload.as_bytes()).await?;
            stdio.stdin.write_all(b"\n").await?;
            stdio.stdin.flush().await?;

            loop {
                let mut buf = String::new();
                let n = stdio.stdout.read_line(&mut buf).await?;
                if n == 0 {
                    bail!("child closed stdout before responding to request id {id}");
                }
                let resp: serde_json::Value = match serde_json::from_str(buf.trim_end()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if resp.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                    return Ok(buf.trim_end().to_string());
                }
                // Not ours — a notification or an unrelated line; keep reading.
            }
        };

        match tokio::time::timeout(timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(anyhow!("request id {id} timed out after {timeout:?}")),
        }
    }

    /// Send a JSON-RPC notification — a message with no `id` and no response to
    /// await. Written under the stdio lock so it never interleaves with a
    /// concurrent [`request`](Child::request) or [`notify`](Child::notify).
    pub async fn notify(&self, line: &str) -> Result<()> {
        let mut stdio = self.stdio.lock().await;
        stdio.stdin.write_all(line.as_bytes()).await?;
        stdio.stdin.write_all(b"\n").await?;
        stdio.stdin.flush().await?;
        Ok(())
    }

    /// Write `line` to the child's stdin followed by a newline, then flush. Use
    /// for newline-delimited protocols (e.g. JSON-RPC over stdio). Acquires the
    /// internal stdio lock, so it is safe alongside the request/response API.
    #[cfg(test)]
    pub async fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut stdio = self.stdio.lock().await;
        stdio.stdin.write_all(line.as_bytes()).await?;
        stdio.stdin.write_all(b"\n").await?;
        stdio.stdin.flush().await
    }

    /// Read one newline-terminated line from the child's stdout, appending into
    /// `buf` (including the trailing newline, matching [`AsyncBufReadExt::read_line`]).
    /// Returns the number of bytes read; `Ok(0)` signals EOF (the child closed
    /// its stdout). Acquires the internal stdio lock.
    #[cfg(test)]
    pub async fn read_line(&self, buf: &mut String) -> std::io::Result<usize> {
        let mut stdio = self.stdio.lock().await;
        stdio.stdout.read_line(buf).await
    }

    /// Request the child be killed and reaped. Idempotent; `kill_on_drop` also
    /// covers the drop path, so this is only needed for an explicit early stop.
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill().await
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

    #[tokio::test]
    async fn spawn_round_trips_a_line_through_the_child() {
        // `cat` echoes each stdin line to stdout — the minimal persistent peer.
        let child = Command::new("cat").spawn().expect("spawn cat");
        child.write_line("ping").await.expect("write line");
        let mut buf = String::new();
        let n = child.read_line(&mut buf).await.expect("read line");
        assert_eq!(n, 5, "expected 'ping\\n'");
        assert_eq!(buf, "ping\n");
    }

    #[tokio::test]
    async fn spawn_read_line_returns_zero_at_eof() {
        let child = Command::new("true").spawn().expect("spawn true");
        let mut buf = String::new();
        let n = child.read_line(&mut buf).await.expect("read line");
        assert_eq!(n, 0, "expected EOF → 0 bytes");
    }

    #[tokio::test]
    async fn request_injects_and_correlates_id() {
        // `cat` echoes each request line verbatim, injected id included — so the
        // echoed response's id matches the one `request` minted.
        let child = Command::new("cat").spawn().expect("spawn cat");
        let resp = child
            .request(
                r#"{"jsonrpc":"2.0","method":"ping"}"#,
                Duration::from_secs(5),
            )
            .await
            .expect("request round-trip");
        let v: serde_json::Value = serde_json::from_str(&resp).expect("parse response");
        assert_eq!(v["id"], serde_json::json!(1));
        assert_eq!(v["method"], serde_json::json!("ping"));
    }

    #[tokio::test]
    async fn request_times_out_when_peer_is_silent() {
        // `sleep` never writes to stdout, so the read side never completes.
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let err = child
            .request(r#"{"method":"ping"}"#, Duration::from_millis(50))
            .await
            .expect_err("expected timeout");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    // Proves no cross-talk: 50 concurrent `request` calls against one `cat`
    // echo peer each get back the response carrying THEIR OWN minted id. If the
    // stdio lock or id correlation were wrong, responses would be swapped and
    // the per-task id assertion would fail.
    #[tokio::test]
    async fn concurrent_requests_never_cross_talk() {
        use std::sync::Arc;

        let child = Arc::new(Command::new("cat").spawn().expect("spawn cat"));
        let mut handles = Vec::new();
        for i in 0..50u64 {
            let child = Arc::clone(&child);
            handles.push(tokio::spawn(async move {
                let line = format!(r#"{{"jsonrpc":"2.0","method":"echo","marker":{i}}}"#);
                let resp = child
                    .request(&line, Duration::from_secs(10))
                    .await
                    .expect("request");
                let v: serde_json::Value = serde_json::from_str(&resp).expect("parse");
                // The response must carry the marker we sent AND an id — and the
                // id the peer echoed must equal the id injected for that marker.
                (v["marker"].as_u64().unwrap(), v["id"].as_u64().unwrap())
            }));
        }

        let mut markers = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for h in handles {
            let (marker, id) = h.await.expect("task join");
            assert!(
                markers.insert(marker),
                "duplicate marker {marker} — cross-talk"
            );
            assert!(ids.insert(id), "duplicate id {id} — cross-talk");
        }
        assert_eq!(markers.len(), 50);
        assert_eq!(ids.len(), 50);
    }
}
