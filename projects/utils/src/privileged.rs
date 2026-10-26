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

pub const STORAGE_APPLY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const CRON_APPLY_TIMEOUT: Duration = Duration::from_secs(45);
/// Bounded so a hung `pct exec` never piles up in D-state and starves the host watchdog.
pub const LXC_EXEC_TIMEOUT: Duration = Duration::from_secs(300);
pub const LXC_EXEC_HELPER_GRACE: Duration = Duration::from_secs(30);
pub const LXC_EXEC_HELPER_TIMEOUT: Duration =
    Duration::from_secs(LXC_EXEC_TIMEOUT.as_secs() + LXC_EXEC_HELPER_GRACE.as_secs());
pub const LXC_PUSH_HELPER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const PLUGIN_TRUST_TIMEOUT: Duration = Duration::from_secs(30);
pub const PLUGIN_OP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Outlasts the root side's [`PLUGIN_OP_TIMEOUT`] so the caller never kills a run root still allows.
pub const PLUGIN_APPLY_TIMEOUT: Duration = Duration::from_secs(PLUGIN_OP_TIMEOUT.as_secs() + 60);
pub const PLUGIN_HELPER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const UNIT_MIGRATE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

pub const ROOT_STORAGE_APPLY_TIMEOUT: Duration = Duration::from_secs(4 * 60 + 30);
pub const ROOT_CRON_APPLY_TIMEOUT: Duration = Duration::from_secs(40);
pub const ROOT_LXC_PUSH_TIMEOUT: Duration = Duration::from_secs(9 * 60);
pub const ROOT_PLUGIN_TRUST_TIMEOUT: Duration = Duration::from_secs(25);
pub const ROOT_UNIT_MIGRATE_TIMEOUT: Duration = Duration::from_secs(100);

/// How long a timed-out helper gets after SIGTERM (which sudo relays) before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_secs(10);

pub fn arm_root_deadline(operation: &'static str, limit: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        eprintln!("orca admin {operation}: exceeded its root-side limit of {limit:?}; exiting");
        std::process::exit(124);
    });
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let status = match tokio::time::timeout(call.timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            terminate(&mut child, TERM_GRACE).await;
            return Err(timed_out(&call.operation(), call.timeout));
        }
    };
    Ok(PrivilegedOutput {
        success: status.success(),
        code: status.code(),
        status: status.to_string(),
        stdout: stdout.await.unwrap_or_default(),
        stderr: stderr.await.unwrap_or_default(),
    })
}

pub async fn terminate(child: &mut tokio::process::Child, grace: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
        // SAFETY: kill(2) on our own child's pid has no memory-safety preconditions.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _killed = child.kill().await;
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
        for (root, caller) in [
            (PLUGIN_OP_TIMEOUT, PLUGIN_APPLY_TIMEOUT),
            (LXC_EXEC_TIMEOUT, LXC_EXEC_HELPER_TIMEOUT),
            (ROOT_STORAGE_APPLY_TIMEOUT, STORAGE_APPLY_TIMEOUT),
            (ROOT_CRON_APPLY_TIMEOUT, CRON_APPLY_TIMEOUT),
            (ROOT_LXC_PUSH_TIMEOUT, LXC_PUSH_HELPER_TIMEOUT),
            (ROOT_PLUGIN_TRUST_TIMEOUT, PLUGIN_TRUST_TIMEOUT),
            (ROOT_UNIT_MIGRATE_TIMEOUT, UNIT_MIGRATE_TIMEOUT),
        ] {
            assert!(
                root < caller,
                "root {root:?} must be shorter than caller {caller:?}"
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
        terminate(&mut child, Duration::from_secs(5)).await;
        let status = child.wait().await.unwrap();
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
        terminate(&mut stubborn, Duration::from_millis(300)).await;
        let status = stubborn.wait().await.unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(started.elapsed() < Duration::from_secs(5));
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
