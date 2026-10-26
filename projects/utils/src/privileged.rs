use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedCall {
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivilegedOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub status: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub type PrivilegedFuture = Pin<Box<dyn Future<Output = std::io::Result<PrivilegedOutput>> + Send>>;

pub trait PrivilegedExec: Send + Sync {
    fn exec(&self, call: PrivilegedCall) -> PrivilegedFuture;
}

/// Bounded so a hung `pct exec` never piles up in D-state and starves the host watchdog.
pub const LXC_EXEC_TIMEOUT: Duration = Duration::from_secs(300);
pub const LXC_EXEC_HELPER_GRACE: Duration = Duration::from_secs(30);
pub const LXC_EXEC_HELPER_TIMEOUT: Duration =
    Duration::from_secs(LXC_EXEC_TIMEOUT.as_secs() + LXC_EXEC_HELPER_GRACE.as_secs());
pub const PLUGIN_OP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Outlasts the root side's [`PLUGIN_OP_TIMEOUT`] so the caller never kills a run root still allows.
pub const PLUGIN_APPLY_TIMEOUT: Duration = Duration::from_secs(PLUGIN_OP_TIMEOUT.as_secs() + 60);
pub const PLUGIN_HELPER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Declares a (root-side, caller-side) timeout pair in one place and folds it
/// into [`TIMEOUT_PAIRS`] at the same time, so a new pair can never be added
/// to one without the other — unlike `LXC_EXEC_HELPER_TIMEOUT`/`PLUGIN_APPLY_TIMEOUT`
/// above, whose root<caller relationship is enforced structurally by their own
/// definitions, every pair declared here is two independently chosen numbers
/// that only a test can police. See `every_root_timeout_is_declared_through_the_macro`
/// and `caller_timeouts_outlast_the_root_side_limits`.
macro_rules! privileged_timeouts {
    ($( $root:ident / $caller:ident => $op:literal, root = $root_v:expr, caller = $caller_v:expr; )+) => {
        $(
            pub const $root: Duration = $root_v;
            pub const $caller: Duration = $caller_v;
        )+
        pub const TIMEOUT_PAIRS: &[(&str, Duration, Duration)] = &[
            $( ($op, $root, $caller), )+
        ];
    };
}

privileged_timeouts! {
    ROOT_STORAGE_APPLY_TIMEOUT / STORAGE_APPLY_TIMEOUT =>
        "storage-apply", root = Duration::from_secs(4 * 60 + 30), caller = Duration::from_secs(5 * 60);
    ROOT_CRON_APPLY_TIMEOUT / CRON_APPLY_TIMEOUT =>
        "cron-apply", root = Duration::from_secs(40), caller = Duration::from_secs(45);
    ROOT_LXC_PUSH_TIMEOUT / LXC_PUSH_HELPER_TIMEOUT =>
        "lxc-push", root = Duration::from_secs(9 * 60), caller = Duration::from_secs(10 * 60);
    ROOT_PLUGIN_TRUST_TIMEOUT / PLUGIN_TRUST_TIMEOUT =>
        "plugin-trust-signed", root = Duration::from_secs(25), caller = Duration::from_secs(30);
    ROOT_UNIT_MIGRATE_TIMEOUT / UNIT_MIGRATE_TIMEOUT =>
        "unit-migrate", root = Duration::from_secs(100), caller = Duration::from_secs(2 * 60);
}

/// How long a timed-out helper gets after SIGTERM (which sudo relays) before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(10);

/// Cancels the root-side watchdog ([`arm_root_deadline`]) when dropped, so a
/// helper that finishes within its own limit is never force-exited afterward
/// by a watchdog thread that doesn't know the work already succeeded.
pub struct RootDeadlineGuard {
    _cancel: std::sync::mpsc::Sender<()>,
}

pub fn arm_root_deadline(operation: &'static str, limit: Duration) -> RootDeadlineGuard {
    arm_root_deadline_with(operation, limit, || std::process::exit(124))
}

fn arm_root_deadline_with(
    operation: &'static str,
    limit: Duration,
    on_expired: impl FnOnce() + Send + 'static,
) -> RootDeadlineGuard {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
            eprintln!("orca admin {operation}: exceeded its root-side limit of {limit:?}; exiting");
            on_expired();
        }
    });
    RootDeadlineGuard { _cancel: tx }
}

/// Last byte a privileged helper writes to stdout, right after it has
/// produced its real result and before the process begins teardown. Lets the
/// caller (`run_child`) tell "the op genuinely finished right at the deadline"
/// apart from "the op was still working when we killed it" — a distinction a
/// bare process exit status can't make, since SIGTERM and natural completion
/// can race at the same instant. Stripped from `PrivilegedOutput::stdout`
/// before it ever reaches a caller, so the wire format callers parse
/// (`serde_json::from_slice`) never sees it.
const DONE_MARKER: u8 = 0x04;

/// Called by every `orca admin <op>` handler right after it has produced its
/// result, so [`run_child`] can tell a genuine finish from a kill mid-flight.
pub fn mark_done() -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    out.write_all(&[DONE_MARKER])?;
    out.flush()
}

fn timed_out(operation: &str, limit: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("privileged `{operation}` did not finish within {limit:?}"),
    )
}

impl PrivilegedCall {
    pub fn new<I, S>(argv: I, stdin: impl Into<Vec<u8>>, timeout: Duration) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            stdin: stdin.into(),
            timeout,
        }
    }

    pub fn helper(subcommand: &str, stdin: impl Into<Vec<u8>>, timeout: Duration) -> Self {
        Self::new(
            [crate::path::PRIVILEGED_BIN, "admin", subcommand],
            stdin,
            timeout,
        )
    }

    fn operation(&self) -> String {
        match self.argv.as_slice() {
            [bin, rest @ ..] if bin == crate::path::PRIVILEGED_BIN && !rest.is_empty() => {
                rest.join(" ")
            }
            argv => argv.join(" "),
        }
    }
}

pub const REFUSE_ENV: &str = "ORCA_PRIVILEGED_REFUSE";

pub const HARNESS_ENVS: [&str; 3] = ["CARGO_TARGET_TMPDIR", "NEXTEST", "NEXTEST_RUN_ID"];

const SUDO_PATHS: [&str; 3] = ["/usr/bin/sudo", "/bin/sudo", "/usr/local/bin/sudo"];

pub async fn run(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    #[cfg(any(test, feature = "test-scope"))]
    {
        let limit = call.timeout;
        let operation = call.operation();
        tokio::time::timeout(limit, testing::dispatch(call))
            .await
            .unwrap_or_else(|_| Err(timed_out(&operation, limit)))
    }
    #[cfg(not(any(test, feature = "test-scope")))]
    {
        Sudo.exec(call).await
    }
}

pub fn under_test_harness() -> bool {
    harness_reason().is_some()
}

fn harness_reason() -> Option<String> {
    harness_reason_from(
        |k| std::env::var_os(k).is_some(),
        std::env::current_exe().ok().as_deref(),
    )
}

fn harness_reason_from(
    env_set: impl Fn(&str) -> bool,
    exe: Option<&std::path::Path>,
) -> Option<String> {
    if let Some(k) = HARNESS_ENVS.iter().find(|k| env_set(k)) {
        return Some(format!("{k} is set"));
    }
    exe.filter(|p| is_cargo_deps_binary(p))
        .map(|p| format!("{} is a cargo test binary", p.display()))
}

pub fn ensure_shippable() -> std::io::Result<()> {
    shippable(crate::TEST_SCOPE, under_test_harness())
}

fn shippable(test_scope: bool, harness: bool) -> std::io::Result<()> {
    if test_scope && !harness {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "this binary was built with orca-utils `test-scope` and must not run outside a test harness",
        ));
    }
    Ok(())
}

fn is_cargo_deps_binary(exe: &std::path::Path) -> bool {
    let Some(dir) = exe.parent() else {
        return false;
    };
    dir.file_name() == Some("deps".as_ref())
        && dir
            .ancestors()
            .any(|a| a.file_name() == Some("target".as_ref()))
}

pub fn refuse_host_mutation(program: &str) -> std::io::Result<()> {
    let reason = if cfg!(any(test, feature = "test-scope")) {
        Some("this is a test build".to_string())
    } else if std::env::var_os(REFUSE_ENV).is_some() {
        Some(format!("{REFUSE_ENV} is set"))
    } else {
        harness_reason()
    };
    match reason {
        Some(r) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to run host-mutating `{program}`: {r}"),
        )),
        None => Ok(()),
    }
}

pub fn host_command_sync(program: &str) -> std::io::Result<std::process::Command> {
    refuse_host_mutation(program)?;
    Ok(std::process::Command::new(program))
}

pub fn host_command(program: &str) -> std::io::Result<tokio::process::Command> {
    refuse_host_mutation(program)?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.kill_on_drop(true);
    Ok(cmd)
}

fn refuse_elevation(call: &PrivilegedCall) -> std::io::Result<()> {
    let program = call.argv.first().map(String::as_str).unwrap_or_default();
    refuse_host_mutation(program)?;
    if program == crate::path::PRIVILEGED_BIN {
        crate::path::privileged_exe()?;
    }
    check_trusted(std::path::Path::new(program)).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to elevate `{program}`: {e}"),
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustMeta {
    pub symlink: bool,
    pub uid: u32,
    pub mode: u32,
}

pub fn check_trusted(p: &std::path::Path) -> Result<(), String> {
    check_trusted_with(p, |path| {
        let meta = std::fs::symlink_metadata(path)?;
        #[cfg(unix)]
        let (uid, mode) = {
            use std::os::unix::fs::MetadataExt;
            (meta.uid(), meta.mode())
        };
        #[cfg(not(unix))]
        let (uid, mode) = (0, 0);
        Ok(TrustMeta {
            symlink: meta.file_type().is_symlink(),
            uid,
            mode,
        })
    })
}

fn check_trusted_with(
    p: &std::path::Path,
    meta: impl Fn(&std::path::Path) -> std::io::Result<TrustMeta>,
) -> Result<(), String> {
    if !p.is_absolute() {
        return Err(format!("{} is not an absolute path", p.display()));
    }
    for path in p.ancestors() {
        let m = meta(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if m.symlink {
            return Err(format!("{} is a symlink", path.display()));
        }
        if m.uid != 0 {
            return Err(format!(
                "{} is owned by uid {}, not root",
                path.display(),
                m.uid
            ));
        }
        if m.mode & 0o022 != 0 {
            return Err(format!(
                "{} has mode {:o}, writable by group or other",
                path.display(),
                m.mode & 0o7777
            ));
        }
    }
    Ok(())
}

fn sudo_binary(candidates: &[&str]) -> std::io::Result<std::path::PathBuf> {
    let sudo = candidates
        .iter()
        .map(std::path::Path::new)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no sudo found at any of {}", candidates.join(", ")),
            )
        })?;
    check_trusted(sudo)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    Ok(sudo.to_path_buf())
}

pub struct Sudo;

impl PrivilegedExec for Sudo {
    fn exec(&self, call: PrivilegedCall) -> PrivilegedFuture {
        Box::pin(async move {
            if !cfg!(target_os = "linux") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "privileged `{}` is Linux-only; orca does not elevate on this OS",
                        call.operation()
                    ),
                ));
            }
            refuse_elevation(&call)?;
            spawn_sudo(call).await
        })
    }
}

#[cfg(any(test, feature = "test-scope"))]
async fn spawn_sudo(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    sudo_binary(&SUDO_PATHS)?;
    Err(testing::refused(&call))
}

#[cfg(not(any(test, feature = "test-scope")))]
async fn spawn_sudo(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(sudo_binary(&SUDO_PATHS)?)
        .arg("-n")
        .args(&call.argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&call.stdin).await?;
        stdin.shutdown().await?;
    }
    run_child(child, call.timeout, &call.operation()).await
}

/// Drains `child`'s stdout/stderr and races its exit against `timeout`. Never
/// gated by `cfg(test)` so the race-handling logic below (the actual thing
/// [`PrivilegedExec::exec`] implementations are trusted to get right) is
/// exercised by ordinary `cargo test`, not compiled away by it — `spawn_sudo`
/// is the only piece that's test-gated, for resolving the trusted sudo path.
/// A plain `--features test-scope` build with no `cfg(test)` has no caller
/// (the mock `spawn_sudo` above never calls it, and `mod tests` isn't
/// compiled), so it's allowed dead in exactly that combination.
#[cfg_attr(all(feature = "test-scope", not(test)), allow(dead_code))]
async fn run_child(
    mut child: tokio::process::Child,
    timeout: Duration,
    operation: &str,
) -> std::io::Result<PrivilegedOutput> {
    use tokio::io::AsyncReadExt;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let stdout = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _read = p.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _read = p.read_to_end(&mut buf).await;
        }
        buf
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        // The timer and the child's real exit can become ready in the same
        // instant — `tokio::time::timeout` then picks a branch essentially at
        // random, so `Err` here does NOT mean the child is still running.
        // `try_wait` catches the case where it had, in fact, already exited.
        Err(_) => match child.try_wait()? {
            Some(status) => status,
            None => {
                // Reaps the child one way or another (SIGTERM, then SIGKILL
                // after TERM_GRACE) — its exit status is ignored for the
                // marker case below: a process killed by a signal always
                // reports `success() == false`, even when it had already
                // written its real result and marker and was merely slow to
                // actually exit.
                let _reaped = terminate(&mut child, TERM_GRACE).await?;
                let mut out_so_far = stdout.await.unwrap_or_default();
                // The helper writes `DONE_MARKER` only after producing its
                // real result, so finding it means the op genuinely finished
                // right at the deadline and was killed only on its way out —
                // not mid-operation. Report that as success, not a timeout.
                if out_so_far.last() == Some(&DONE_MARKER) {
                    out_so_far.pop();
                    return Ok(PrivilegedOutput {
                        success: true,
                        code: None,
                        status: "completed before being reaped".into(),
                        stdout: out_so_far,
                        stderr: stderr.await.unwrap_or_default(),
                    });
                }
                return Err(timed_out(operation, timeout));
            }
        },
    };
    Ok(finish(
        status,
        stdout.await.unwrap_or_default(),
        stderr.await.unwrap_or_default(),
    ))
}

#[cfg_attr(all(feature = "test-scope", not(test)), allow(dead_code))]
fn finish(
    status: std::process::ExitStatus,
    mut stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> PrivilegedOutput {
    if stdout.last() == Some(&DONE_MARKER) {
        stdout.pop();
    }
    PrivilegedOutput {
        success: status.success(),
        code: status.code(),
        status: status.to_string(),
        stdout,
        stderr,
    }
}

/// SIGTERMs `child` (sudo relays it to the real helper) and waits up to
/// `grace` for it to exit on its own before SIGKILLing it. Always reaps the
/// child and returns its real exit status — the caller decides what that
/// status means (e.g. by checking [`DONE_MARKER`]).
pub async fn terminate(
    child: &mut tokio::process::Child,
    grace: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
        // SAFETY: kill(2) on our own child's pid has no memory-safety preconditions.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if let Ok(status) = tokio::time::timeout(grace, child.wait()).await {
            return status;
        }
    }
    child.kill().await?;
    child.wait().await
}

#[cfg(any(test, feature = "test-scope"))]
pub mod testing {
    use super::{PrivilegedCall, PrivilegedExec, PrivilegedFuture, PrivilegedOutput};
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    tokio::task_local! {
        static EXEC: Arc<dyn PrivilegedExec>;
    }

    pub(super) fn refused(call: &PrivilegedCall) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "privileged exec is refused in test builds (argv: {:?}); install a fake with utils::privileged::testing::with_exec",
                call.argv
            ),
        )
    }

    pub(super) async fn dispatch(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
        match EXEC.try_with(Arc::clone) {
            Ok(exec) => exec.exec(call).await,
            Err(_) => Err(refused(&call)),
        }
    }

    pub async fn with_exec<F: Future>(exec: Arc<dyn PrivilegedExec>, f: F) -> F::Output {
        EXEC.scope(exec, f).await
    }

    pub struct Recorder {
        calls: Mutex<Vec<PrivilegedCall>>,
        reply: PrivilegedOutput,
    }

    impl Recorder {
        pub fn new(reply: PrivilegedOutput) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                reply,
            })
        }

        pub fn calls(&self) -> Vec<PrivilegedCall> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl PrivilegedExec for Recorder {
        fn exec(&self, call: PrivilegedCall) -> PrivilegedFuture {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(call);
            let reply = self.reply.clone();
            Box::pin(async move { Ok(reply) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Recorder, with_exec};
    use super::*;

    #[tokio::test]
    async fn run_without_a_fake_refuses_and_never_spawns_sudo() {
        let err = run(PrivilegedCall::new(
            ["/bin/true"],
            Vec::new(),
            STORAGE_APPLY_TIMEOUT,
        ))
        .await
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_real_sudo_executor_is_a_hard_error_in_test_builds() {
        let err = Sudo
            .exec(PrivilegedCall::new(
                ["/bin/true"],
                Vec::new(),
                STORAGE_APPLY_TIMEOUT,
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
        ));
    }

    #[test]
    fn host_mutation_is_refused_in_test_builds() {
        let err = refuse_host_mutation("pct").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("pct"), "{err}");
    }

    #[test]
    fn the_test_process_is_detected_as_a_harness() {
        assert!(under_test_harness());
    }

    #[test]
    fn a_test_scope_binary_refuses_to_start_outside_a_harness() {
        assert!(shippable(true, false).is_err());
        assert!(shippable(true, true).is_ok());
        assert!(shippable(false, false).is_ok());
    }

    #[test]
    fn cargo_deps_binaries_are_test_harnesses() {
        assert!(is_cargo_deps_binary(std::path::Path::new(
            "/w/target/debug/deps/orca_system-abc"
        )));
        assert!(is_cargo_deps_binary(std::path::Path::new(
            "/w/target/x86_64-unknown-linux-gnu/debug/deps/t-1"
        )));
        assert!(!is_cargo_deps_binary(std::path::Path::new(
            "/w/target/debug/orca"
        )));
        assert!(!is_cargo_deps_binary(std::path::Path::new(
            "/usr/local/libexec/orca/orca"
        )));
    }

    fn fake_meta(
        bad: &'static str,
        meta: TrustMeta,
    ) -> impl Fn(&std::path::Path) -> std::io::Result<TrustMeta> {
        move |p| {
            Ok(if p == std::path::Path::new(bad) {
                meta
            } else {
                TrustMeta {
                    symlink: false,
                    uid: 0,
                    mode: 0o755,
                }
            })
        }
    }

    #[test]
    fn trust_requires_a_root_owned_unwritable_real_chain() {
        let bin = std::path::Path::new("/usr/local/libexec/orca/orca");
        let ok = TrustMeta {
            symlink: false,
            uid: 0,
            mode: 0o755,
        };
        assert!(check_trusted_with(bin, fake_meta("/none", ok)).is_ok());
        let e = check_trusted_with(
            bin,
            fake_meta(
                "/usr/local/libexec/orca/orca",
                TrustMeta { uid: 1000, ..ok },
            ),
        )
        .unwrap_err();
        assert!(
            e.contains("/usr/local/libexec/orca/orca is owned by uid 1000"),
            "{e}"
        );
        let e = check_trusted_with(
            bin,
            fake_meta("/usr/local", TrustMeta { mode: 0o775, ..ok }),
        )
        .unwrap_err();
        assert!(e.contains("/usr/local has mode 775"), "{e}");
        assert!(check_trusted_with(bin, fake_meta("/", TrustMeta { mode: 0o757, ..ok })).is_err());
        assert!(
            check_trusted_with(bin, fake_meta("/usr", TrustMeta { mode: 0o1755, ..ok })).is_ok()
        );
        let e = check_trusted_with(
            bin,
            fake_meta(
                "/usr/local/libexec",
                TrustMeta {
                    symlink: true,
                    ..ok
                },
            ),
        )
        .unwrap_err();
        assert!(e.contains("/usr/local/libexec is a symlink"), "{e}");
        let e =
            check_trusted_with(std::path::Path::new("orca"), fake_meta("/none", ok)).unwrap_err();
        assert!(e.contains("orca is not an absolute path"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn trust_reads_real_files() {
        assert!(check_trusted(std::path::Path::new("/nonexistent/orca")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("orca");
        std::fs::write(&bin, b"").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let e = check_trusted(&link).unwrap_err();
        assert!(e.contains("link is a symlink"), "{e}");
        assert!(check_trusted(std::path::Path::new("/usr/bin/env")).is_ok());
    }

    #[test]
    fn sudo_is_only_taken_from_the_fixed_list() {
        let err = sudo_binary(&["/nonexistent/sudo", "/also/missing/sudo"]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("/nonexistent/sudo"), "{err}");
        assert!(
            SUDO_PATHS
                .iter()
                .all(|p| std::path::Path::new(p).is_absolute())
        );
    }

    #[test]
    fn the_harness_reason_names_the_env_var_or_binary() {
        let none = |_: &str| false;
        assert_eq!(
            harness_reason_from(|k| k == "NEXTEST_RUN_ID", None).as_deref(),
            Some("NEXTEST_RUN_ID is set")
        );
        assert_eq!(
            harness_reason_from(|k| k == "RUST_TEST_THREADS", None),
            None
        );
        let exe = std::path::Path::new("/w/target/debug/deps/t-1");
        assert_eq!(
            harness_reason_from(none, Some(exe)).as_deref(),
            Some("/w/target/debug/deps/t-1 is a cargo test binary")
        );
        assert_eq!(
            harness_reason_from(none, Some(std::path::Path::new("/usr/local/bin/orca"))),
            None
        );
    }

    #[test]
    fn caller_timeouts_outlast_the_root_side_limits() {
        for (op, root, caller) in TIMEOUT_PAIRS.iter().copied() {
            assert!(
                root < caller,
                "{op}: root {root:?} must be shorter than caller {caller:?}"
            );
        }
        for (root, caller) in [
            (PLUGIN_OP_TIMEOUT, PLUGIN_APPLY_TIMEOUT),
            (LXC_EXEC_TIMEOUT, LXC_EXEC_HELPER_TIMEOUT),
        ] {
            assert!(
                root < caller,
                "root {root:?} must be shorter than caller {caller:?}"
            );
        }
    }

    #[test]
    fn every_root_timeout_is_declared_through_the_macro_not_bare() {
        // `privileged_timeouts!` is the only place a `ROOT_*_TIMEOUT` may be
        // declared — it folds every pair into `TIMEOUT_PAIRS` by construction,
        // so `caller_timeouts_outlast_the_root_side_limits` can't miss one. A
        // bare `pub const ROOT_...` added outside the macro would skip that
        // check silently; this scan catches it.
        let src = include_str!("privileged.rs");
        for line in src.lines() {
            let line = line.trim();
            assert!(
                !line.starts_with("pub const ROOT_"),
                "found a bare `{line}` outside privileged_timeouts!; every \
                 ROOT_*_TIMEOUT must be declared through that macro so it is \
                 paired with a caller timeout and checked"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_sends_sigterm_before_sigkill() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = terminate(&mut child, Duration::from_secs(5)).await.unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));

        let mut stubborn = tokio::process::Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 0.1; done"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = std::time::Instant::now();
        let status = terminate(&mut stubborn, Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    fn piped_child(script: &str) -> tokio::process::Child {
        tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn a_child_that_finishes_in_time_is_reported_as_success() {
        let child = piped_child("printf ok");
        let out = run_child(child, Duration::from_secs(5), "test-op")
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, b"ok");
    }

    /// The exact race N1 guards against: the op finishes and writes its
    /// `DONE_MARKER` right as the caller's deadline elapses, then the process
    /// is slow to actually exit afterward. Deterministic (no timing margin to
    /// get unlucky on): the marker arrives well before the timeout fires, and
    /// the process is only reaped afterward, by `terminate`, exercising the
    /// exact path the fix adds — checking the marker even after a kill.
    #[tokio::test]
    async fn a_child_that_wrote_its_done_marker_before_being_terminated_is_success() {
        let script = format!("printf ok; printf '\\{:03o}'; sleep 5", DONE_MARKER);
        let child = piped_child(&script);
        let out = run_child(child, Duration::from_millis(50), "test-op")
            .await
            .unwrap();
        assert!(out.success, "{out:?}");
        assert_eq!(out.stdout, b"ok");
    }

    #[tokio::test]
    async fn a_child_killed_before_writing_its_done_marker_is_a_real_timeout() {
        let child = piped_child("sleep 5");
        let err = run_child(child, Duration::from_millis(50), "test-op")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("test-op"), "{err}");
    }

    #[test]
    fn dropping_the_guard_before_the_deadline_cancels_the_watchdog() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = fired.clone();
        {
            let _guard = arm_root_deadline_with("test-op", Duration::from_millis(60), move || {
                f.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            // Dropped here, well before the deadline — simulating a helper
            // that finished its real work in time.
        }
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "a successful completion must never be force-exited after the fact"
        );
    }

    #[test]
    fn an_unfinished_op_past_its_deadline_fires_the_watchdog() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = fired.clone();
        let guard = arm_root_deadline_with("test-op", Duration::from_millis(20), move || {
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        drop(guard);
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn elevation_is_linux_only() {
        let err = Sudo
            .exec(PrivilegedCall::helper(
                "storage-apply",
                Vec::new(),
                STORAGE_APPLY_TIMEOUT,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("Linux-only"), "{err}");
    }

    #[test]
    fn host_command_is_the_refusing_seam_for_host_mutation() {
        let err = host_command("docker").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("`docker`"), "{err}");
    }

    struct Sleeper;

    impl PrivilegedExec for Sleeper {
        fn exec(&self, _call: PrivilegedCall) -> PrivilegedFuture {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(PrivilegedOutput::default())
            })
        }
    }

    #[tokio::test]
    async fn the_seam_enforces_the_call_timeout_and_names_the_operation() {
        let started = std::time::Instant::now();
        let err = with_exec(
            std::sync::Arc::new(Sleeper),
            run(PrivilegedCall::helper(
                "storage-apply",
                Vec::new(),
                Duration::from_millis(50),
            )),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let msg = err.to_string();
        assert!(msg.contains("admin storage-apply"), "{msg}");
        assert!(msg.contains("within 50ms"), "{msg}");
    }

    #[tokio::test]
    async fn a_scoped_fake_records_the_call() {
        let rec = Recorder::new(PrivilegedOutput {
            success: true,
            stdout: b"ok".to_vec(),
            ..Default::default()
        });
        let out = with_exec(
            rec.clone(),
            run(PrivilegedCall::new(
                ["orca", "admin", "x"],
                b"in".to_vec(),
                STORAGE_APPLY_TIMEOUT,
            )),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, b"ok");
        assert_eq!(
            rec.calls(),
            vec![PrivilegedCall::new(
                ["orca", "admin", "x"],
                b"in".to_vec(),
                STORAGE_APPLY_TIMEOUT
            )]
        );
    }
}
