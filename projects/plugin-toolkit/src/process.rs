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

/// A failed shell-out through [`Command::run_checked`]: which tool ran, its exit
/// code (`None` when killed by a signal), and its trimmed stderr. Backends map
/// this into their own error type (`NfsError`/`SmbError`), so the "run a command
/// and fail on non-zero" mechanic lives here once instead of being hand-rolled at
/// every call site. `Display` renders `tool (exit <code>): <stderr>`.
#[derive(Clone, Debug)]
pub struct ToolError {
    pub tool: String,
    pub code: Option<i32>,
    pub stderr: String,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (exit {:?}): {}", self.tool, self.code, self.stderr)
    }
}

impl std::error::Error for ToolError {}

/// A subprocess to run. Builder-style; the child is killed on drop so a dropped
/// or timed-out command never leaks a process.
pub struct Command {
    inner: tokio::process::Command,
    /// The program name, retained so [`run_checked`](Command::run_checked) can
    /// name the tool in a [`ToolError`] without re-plumbing it from the caller.
    program: String,
    kill_group: bool,
}

impl Command {
    /// A command that will run `program`.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref();
        let name = program.to_string_lossy().into_owned();
        let mut inner = tokio::process::Command::new(program);
        inner.kill_on_drop(true);
        Self {
            inner,
            program: name,
            kill_group: false,
        }
    }

    pub fn host(program: &str) -> std::io::Result<Self> {
        utils::privileged::refuse_host_mutation(program)?;
        Ok(Self::new(program))
    }

    /// Start the child in its own process group so a timeout SIGKILLs the whole
    /// group, grandchildren included, as does cancelling the future. A child that
    /// exits in time leaves the group alone, so processes it backgrounded keep
    /// running. Opt-in because a separate group leaves the tty foreground group.
    /// Unix only; elsewhere only the direct child is killed.
    pub fn kill_process_group_on_timeout(mut self) -> Self {
        #[cfg(unix)]
        self.inner.process_group(0);
        self.kill_group = true;
        self
    }

    /// Run to completion and return captured stdout bytes on success, or a
    /// [`ToolError`] (tool name, exit code, trimmed stderr) on a non-zero exit.
    /// The single "shell out and fail on non-zero" mechanic every backend maps
    /// into its own error type, replacing hand-rolled `status.success()` checks.
    pub async fn run_checked(self) -> std::result::Result<Vec<u8>, ToolError> {
        let program = self.program.clone();
        // An I/O failure to even launch the process is surfaced as a ToolError
        // with no exit code, mirroring a killed-by-signal outcome.
        let out = match self.output().await {
            Ok(out) => out,
            Err(e) => {
                return Err(ToolError {
                    tool: program,
                    code: None,
                    stderr: e.to_string(),
                });
            }
        };
        if out.status.success {
            Ok(out.stdout)
        } else {
            Err(ToolError {
                tool: program,
                code: out.status.code,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
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
        let mut guard = GroupGuard::new(self.kill_group, &child);
        match crate::time::timeout(budget, guard.wait_leader(&mut child)).await {
            Some(result) => {
                result?;
                guard.disarm();
                let st = child.wait().await?;
                Ok(Some(ExitStatus {
                    success: st.success(),
                    code: st.code(),
                }))
            }
            None => {
                guard.kill_on_timeout(&mut child);
                child.wait().await?;
                guard.disarm();
                Ok(None)
            }
        }
    }

    /// Run up to `budget`, capturing stdout + stderr. Never returns partial
    /// output as success:
    ///
    /// - The child exits and both pipes reach EOF before the deadline:
    ///   `Ok(Some(output))` with the complete output.
    /// - The child has not exited by the deadline: `Ok(None)`; the child is
    ///   killed and reaped.
    /// - The child exits but a process it left behind still holds a pipe open at
    ///   the deadline: `Err` wrapping a [`PipeHeldOpen`] that carries the child's
    ///   exit status.
    ///
    /// On either deadline path the process group is also killed if
    /// [`kill_process_group_on_timeout`](Self::kill_process_group_on_timeout) was
    /// set; otherwise processes the child left behind keep running.
    pub async fn output_within(mut self, budget: Duration) -> std::io::Result<Option<Output>> {
        self.inner
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = self.inner.spawn()?;
        let stdout = child
            .stdout
            .take()
            .expect("stdout was piped, so it is present");
        let stderr = child
            .stderr
            .take()
            .expect("stderr was piped, so it is present");
        let mut guard = GroupGuard::new(self.kill_group, &child);
        let out_buf = SharedBuf::default();
        let err_buf = SharedBuf::default();
        let mut reader = AbortOnDrop(tokio::spawn(read_pipes(
            stdout,
            stderr,
            out_buf.clone(),
            err_buf.clone(),
        )));
        let deadline = tokio::time::Instant::now() + budget;
        match tokio::time::timeout_at(deadline, guard.wait_leader(&mut child)).await {
            // Guard stays armed so its drop kills an opted-in group.
            Ok(result) => result?,
            Err(_) => {
                reader.abort();
                guard.kill_on_timeout(&mut child);
                child.wait().await?;
                guard.disarm();
                return Ok(None);
            }
        }
        let eof = drain_pipes(&mut reader, deadline).await?;
        if !eof {
            reader.abort();
            guard.kill();
        }
        let status = child.wait().await?;
        guard.disarm();
        let status = ExitStatus {
            success: status.success(),
            code: status.code(),
        };
        if !eof {
            return Err(std::io::Error::other(PipeHeldOpen { status }));
        }
        Ok(Some(Output {
            status,
            stdout: take_buf(&out_buf),
            stderr: take_buf(&err_buf),
        }))
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

/// [`Command::output_within`]'s error when the child exited but its stdout or
/// stderr was still held open, by a process it left behind, at the deadline.
#[derive(Clone, Copy, Debug)]
pub struct PipeHeldOpen {
    pub status: ExitStatus,
}

impl std::fmt::Display for PipeHeldOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "process exited (exit {:?}) but its output pipe was still held open at the deadline",
            self.status.code
        )
    }
}

impl std::error::Error for PipeHeldOpen {}

// `Ok(true)` once both pipes hit EOF, `Ok(false)` if the deadline came first.
async fn drain_pipes<F>(reader: &mut F, deadline: tokio::time::Instant) -> std::io::Result<bool>
where
    F: std::future::Future<
            Output = std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
        > + Unpin,
{
    match tokio::time::timeout_at(deadline, reader).await {
        Ok(pipes) => pipes.map_err(std::io::Error::other)?.map(|()| true),
        Err(_) => Ok(false),
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> AbortOnDrop<T> {
    fn abort(&self) {
        self.0.abort();
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = std::result::Result<T, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type SharedBuf = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

fn take_buf(buf: &SharedBuf) -> Vec<u8> {
    std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()))
}

// Reads into shared buffers so output survives the reader being aborted.
async fn read_pipes(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    out: SharedBuf,
    err: SharedBuf,
) -> std::io::Result<()> {
    let (o, e) = tokio::join!(read_pipe(stdout, out), read_pipe(stderr, err));
    o?;
    e
}

async fn read_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    buf: SharedBuf,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut pipe, &mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&chunk[..n]);
    }
}

struct GroupGuard {
    pgid: Option<u32>,
}

impl GroupGuard {
    fn new(kill_group: bool, child: &tokio::process::Child) -> Self {
        Self {
            pgid: if kill_group && cfg!(unix) {
                child.id()
            } else {
                None
            },
        }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid
            && unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) } != 0
        {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                tracing::debug!(pgid, error = %e, "killpg failed");
            }
        }
    }

    fn kill_on_timeout(&self, child: &mut tokio::process::Child) {
        self.kill();
        drop(child.start_kill());
    }

    async fn wait_leader(&self, child: &mut tokio::process::Child) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            loop {
                if leader_exited(pgid)? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        child.wait().await.map(drop)
    }
}

#[cfg(unix)]
fn leader_exited(pid: u32) -> std::io::Result<bool> {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(true),
            _ => return Err(e),
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
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
    async fn run_checked_returns_stdout_on_success() {
        let out = Command::new("printf")
            .arg("hello")
            .run_checked()
            .await
            .expect("run printf");
        assert_eq!(out, b"hello");
    }

    #[tokio::test]
    async fn run_checked_errors_with_tool_code_and_stderr() {
        // `sh -c 'echo boom >&2; exit 3'` fails with a code and stderr.
        let err = Command::new("sh")
            .arg("-c")
            .arg("echo boom >&2; exit 3")
            .run_checked()
            .await
            .expect_err("expected non-zero exit");
        assert_eq!(err.tool, "sh");
        assert_eq!(err.code, Some(3));
        assert_eq!(err.stderr, "boom");
        assert!(err.to_string().contains("exit Some(3)"));
    }

    #[tokio::test]
    async fn run_checked_missing_program_is_a_tool_error() {
        let err = Command::new("definitely-not-a-real-binary-xyz")
            .run_checked()
            .await
            .expect_err("expected launch failure");
        assert_eq!(err.tool, "definitely-not-a-real-binary-xyz");
        assert_eq!(err.code, None);
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

    /// The leader forks a holder in its group that blocks on a pipe only the
    /// leader writes to. The group kill leaves fd 6 silent; a holder that
    /// outlives the leader sees EOF and reports it there.
    #[cfg(unix)]
    const SURVIVOR_REPORTER: &str = r#"exec perl -MPOSIX -e 'pipe(R, W) or die; my $k = fork; die unless defined $k; if (!$k) { close W; POSIX::close(4); sysread(R, my $b, 1); POSIX::write(6, qq(survived\n), 9); exit } close R; POSIX::close(6); POSIX::write(4, qq($k\n), length qq($k\n)); POSIX::close(4); select(undef, undef, undef, undef)'"#;

    #[cfg(unix)]
    async fn grandchild_dies_after_timeout(run: impl AsyncFnOnce(Command) -> bool) {
        let dir = HolderDir::new("pgkill");
        let dead = dir.death_watch();
        let cmd = dir
            .command(SURVIVOR_REPORTER)
            .kill_process_group_on_timeout();
        let (timed_out, _) = tokio::join!(run(cmd), dir.pid());
        assert!(timed_out, "expected timeout");
        assert_eq!(
            read_to_eof(dead).await,
            "",
            "group member outlived the leader"
        );
    }

    #[cfg(unix)]
    fn process_alive(pid: libc::pid_t) -> bool {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, rest)) = stat.rsplit_once(')')
        {
            return !matches!(rest.trim_start().chars().next(), Some('Z' | 'X'));
        }
        true
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn status_within_kills_process_group_on_timeout() {
        grandchild_dies_after_timeout(async |cmd| {
            cmd.status_within(Duration::from_millis(300))
                .await
                .unwrap()
                .is_none()
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_within_kills_process_group_on_timeout() {
        grandchild_dies_after_timeout(async |cmd| {
            cmd.output_within(Duration::from_millis(300))
                .await
                .unwrap()
                .is_none()
        })
        .await;
    }

    #[cfg(unix)]
    async fn backgrounded_grandchild_survives(
        opt_in: bool,
        run: impl AsyncFnOnce(Command) -> bool,
    ) {
        let dir = HolderDir::new("pgkeep");
        let mut cmd = dir.command("sleep 30 >/dev/null 2>&1 4>&- & echo $! >&4");
        if opt_in {
            cmd = cmd.kill_process_group_on_timeout();
        }
        assert!(run(cmd).await, "expected a successful run");
        let pid = dir.pid().await;
        assert!(process_alive(pid), "backgrounded sleep {pid} was killed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn status_within_leaves_backgrounded_grandchild_alive() {
        for opt_in in [false, true] {
            backgrounded_grandchild_survives(opt_in, async |cmd| {
                cmd.status_within(Duration::from_secs(10))
                    .await
                    .unwrap()
                    .is_some_and(|st| st.success)
            })
            .await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_within_leaves_backgrounded_grandchild_alive() {
        for opt_in in [false, true] {
            backgrounded_grandchild_survives(opt_in, async |cmd| {
                cmd.output_within(Duration::from_secs(10))
                    .await
                    .unwrap()
                    .is_some_and(|out| out.status.success)
            })
            .await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_future_kills_the_group() {
        grandchild_dies_after_timeout(async |cmd| {
            tokio::time::timeout(
                Duration::from_millis(300),
                cmd.status_within(Duration::from_secs(60)),
            )
            .await
            .is_err()
        })
        .await;
    }

    /// The group leader exits only once the holder has left the group (sent its
    /// pid up fd 3), so the group kill can never reach it first; `linger` keeps
    /// the leader alive after that.
    #[cfg(unix)]
    fn detached_holder(dir: &HolderDir, linger: bool) -> Command {
        let script = format!(
            "echo $$ > \"$LEADER\"; exec 7>&1; \
             {{ perl -MPOSIX -e 'POSIX::setsid(); $SIG{{PIPE}}=q(IGNORE); $|=1; \
             POSIX::write(3, qq($$\\n), length qq($$\\n)); POSIX::close(3); \
             for (1..600) {{ unless (print qq(x\\n)) {{ POSIX::write(5, qq(epipe\\n), 6); exit }} \
             select(undef, undef, undef, 0.05) }}' 3>&1 1>&7 7>&- 4>&- & }} \
             | {{ read holder; echo \"$holder\" >&4; }}; {tail}",
            tail = if linger {
                "exec sleep 60 4>&- 5>&- 7>&-"
            } else {
                ":"
            },
        );
        dir.command(&script).kill_process_group_on_timeout()
    }

    /// Both ends close on exec; [`HolderDir::command`] dups the write end into
    /// the child only, so the process under test is its sole writer.
    #[cfg(unix)]
    fn cloexec_pipe() -> (std::fs::File, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::os::fd::OwnedFd::from_raw_fd(fds[1]),
            )
        }
    }

    #[cfg(unix)]
    fn set_nonblocking(file: &std::fs::File, on: bool) {
        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let flags = if on {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    }

    /// Reads byte by byte so a second line on the same pipe is left unread;
    /// returns what was read if every writer closes first.
    #[cfg(unix)]
    async fn read_line(file: &std::fs::File) -> String {
        use std::io::Read;
        let mut file = file.try_clone().unwrap();
        tokio::task::spawn_blocking(move || {
            let mut line = Vec::new();
            let mut byte = [0u8];
            while file.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            String::from_utf8(line).unwrap()
        })
        .await
        .unwrap()
    }

    /// Returns once every process holding the pipe's write end has exited.
    #[cfg(unix)]
    async fn read_to_eof(file: std::fs::File) -> String {
        use std::io::Read;
        let mut file = file;
        tokio::task::spawn_blocking(move || {
            let mut out = String::new();
            file.read_to_string(&mut out).unwrap();
            out
        })
        .await
        .unwrap()
    }

    /// Kills every holder and group leader it has seen and removes the dir, on
    /// every exit path of the test.
    #[cfg(unix)]
    struct HolderDir {
        path: std::path::PathBuf,
        pid: std::fs::File,
        mark: std::fs::File,
        writers: std::sync::Mutex<Vec<(std::os::fd::OwnedFd, libc::c_int)>>,
        seen: std::sync::Mutex<Vec<libc::pid_t>>,
    }

    #[cfg(unix)]
    impl HolderDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", crate::mint_uuidv7()));
            std::fs::create_dir_all(&path).unwrap();
            let (pid, pid_w) = cloexec_pipe();
            let (mark, mark_w) = cloexec_pipe();
            Self {
                pid,
                mark,
                writers: std::sync::Mutex::new(vec![(pid_w, 4), (mark_w, 5)]),
                path,
                seen: Default::default(),
            }
        }

        /// The child gets the pid pipe on fd 4, the mark pipe on fd 5 and the
        /// death watch on fd 6. The parent's write ends drop with the command,
        /// so a child that exits without writing yields EOF.
        fn command(&self, script: &str) -> Command {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let writers = std::mem::take(&mut *self.writers.lock().unwrap());
            let mut cmd = Command::new("sh")
                .arg("-c")
                .arg(script)
                .env("LEADER", self.path.join("leader"));
            // Sources move above every target first so no dup2 clobbers one.
            unsafe {
                cmd.inner.as_std_mut().pre_exec(move || {
                    let mut high = [0; 3];
                    for (slot, (fd, _)) in high.iter_mut().zip(&writers) {
                        *slot = libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10);
                        if *slot < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    for (from, (_, to)) in high.iter().zip(&writers) {
                        if libc::dup2(*from, *to) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            cmd
        }

        fn death_watch(&self) -> std::fs::File {
            let (dead, dead_w) = cloexec_pipe();
            self.writers.lock().unwrap().push((dead_w, 6));
            dead
        }

        async fn pid(&self) -> libc::pid_t {
            let line = read_line(&self.pid).await;
            let pid = line
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("process under test sent no pid: {line:?}"));
            self.seen.lock().unwrap().push(pid);
            pid
        }

        async fn mark(&self) -> String {
            read_line(&self.mark).await
        }
    }

    #[cfg(unix)]
    impl Drop for HolderDir {
        fn drop(&mut self) {
            use std::io::Read;
            let mut pids = std::mem::take(&mut *self.seen.lock().unwrap());
            set_nonblocking(&self.pid, true);
            let mut unread = String::new();
            (&self.pid).read_to_string(&mut unread).ok();
            if let Ok(leader) = std::fs::read_to_string(self.path.join("leader")) {
                unread.push_str(&leader);
            }
            pids.extend(
                unread
                    .lines()
                    .filter_map(|l| l.trim().parse::<libc::pid_t>().ok()),
            );
            for pid in pids {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    #[cfg(unix)]
    async fn kill_holder_once_ready(dir: &HolderDir) {
        let pid = dir.pid().await;
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_within_waits_for_holder_outside_group_to_close_pipe() {
        let dir = HolderDir::new("pghold");
        let (out, ()) = tokio::join!(
            detached_holder(&dir, false).output_within(Duration::from_secs(600)),
            kill_holder_once_ready(&dir),
        );
        let out = out.unwrap().expect("leader exited in time");
        assert!(out.status.success);
    }

    /// The holder sends its pid before the leader exits, so the test kills it
    /// and the pipe then reaches EOF.
    #[cfg(unix)]
    async fn grandchild_holding_stdout(opt_in: bool) {
        let dir = HolderDir::new("pgpipe");
        let mut cmd = dir.command("echo hi; sleep 30 4>&- & echo $! >&4");
        if opt_in {
            cmd = cmd.kill_process_group_on_timeout();
        }
        let (out, ()) = tokio::join!(
            cmd.output_within(Duration::from_secs(600)),
            kill_holder_once_ready(&dir),
        );
        let out = out.unwrap().expect("leader exited in time");
        assert!(out.status.success);
        assert_eq!(out.stdout, b"hi\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_within_returns_full_output_once_grandchild_releases_stdout() {
        grandchild_holding_stdout(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_within_returns_full_output_once_grandchild_in_group_releases_stdout() {
        grandchild_holding_stdout(true).await;
    }

    fn pipe_reader() -> (
        tokio::io::DuplexStream,
        AbortOnDrop<std::io::Result<()>>,
        SharedBuf,
    ) {
        let (writer, pipe) = tokio::io::duplex(64);
        let buf = SharedBuf::default();
        let reader = AbortOnDrop(tokio::spawn(read_pipe(pipe, buf.clone())));
        (writer, reader, buf)
    }

    /// A deadline already in the past makes the timeout fire on its first poll,
    /// so the held-pipe path is reached without waiting on any clock.
    #[tokio::test]
    async fn drain_pipes_reports_held_pipe_at_deadline() {
        let (writer, mut reader, _buf) = pipe_reader();
        let deadline = tokio::time::Instant::now();
        assert!(
            !drain_pipes(&mut reader, deadline).await.unwrap(),
            "held pipe reported as EOF"
        );
        drop(writer);
    }

    #[tokio::test]
    async fn drain_pipes_reports_eof_before_deadline() {
        let (mut writer, mut reader, buf) = pipe_reader();
        writer.write_all(b"all").await.unwrap();
        drop(writer);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        assert!(drain_pipes(&mut reader, deadline).await.unwrap());
        assert_eq!(take_buf(&buf), b"all");
    }

    #[test]
    fn pipe_held_open_error_carries_exit_status() {
        let err = std::io::Error::other(PipeHeldOpen {
            status: ExitStatus {
                success: false,
                code: Some(7),
            },
        });
        let held = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<PipeHeldOpen>())
            .expect("PipeHeldOpen inside the io::Error");
        assert_eq!(held.status.code, Some(7));
        assert!(err.to_string().contains("still held open"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_output_within_aborts_the_pipe_reader() {
        let dir = HolderDir::new("pgcancel");
        let started = tokio::select! {
            _ = detached_holder(&dir, true).output_within(Duration::from_secs(60)) => false,
            _ = dir.pid() => true,
        };
        assert!(started, "expected the holder to start, then the cancel");
        assert_eq!(dir.mark().await, "epipe", "pipe reader leaked after cancel");
    }

    #[tokio::test]
    async fn output_within_captures_fast_command() {
        let out = Command::new("printf")
            .arg("hi")
            .output_within(Duration::from_secs(5))
            .await
            .unwrap()
            .expect("in time");
        assert_eq!(out.stdout, b"hi");
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
