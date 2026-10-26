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
pub const PLUGIN_APPLY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const PLUGIN_HELPER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const UNIT_MIGRATE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

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

const HARNESS_ENVS: [&str; 4] = [
    "CARGO_TARGET_TMPDIR",
    "RUST_TEST_THREADS",
    "NEXTEST",
    "NEXTEST_RUN_ID",
];

pub async fn run(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    let limit = call.timeout;
    let operation = call.operation();
    match tokio::time::timeout(limit, dispatch(call)).await {
        Ok(out) => out,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("privileged `{operation}` did not finish within {limit:?}"),
        )),
    }
}

async fn dispatch(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    #[cfg(any(test, feature = "test-scope"))]
    {
        testing::dispatch(call).await
    }
    #[cfg(not(any(test, feature = "test-scope")))]
    {
        Sudo.exec(call).await
    }
}

pub fn under_test_harness() -> bool {
    HARNESS_ENVS.iter().any(|k| std::env::var_os(k).is_some())
        || std::env::current_exe().is_ok_and(|p| is_cargo_deps_binary(&p))
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
    } else if under_test_harness() {
        Some("the process is a test harness".to_string())
    } else {
        None
    };
    match reason {
        Some(r) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to run host-mutating `{program}`: {r}"),
        )),
        None => Ok(()),
    }
}

fn refuse_elevation(call: &PrivilegedCall) -> std::io::Result<()> {
    let program = call.argv.first().map(String::as_str).unwrap_or_default();
    refuse_host_mutation(program)?;
    if program == crate::path::PRIVILEGED_BIN {
        crate::path::privileged_exe()?;
    }
    trusted_program(std::path::Path::new(program)).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to elevate `{program}`: {e}"),
        )
    })
}

fn trusted_program(p: &std::path::Path) -> Result<(), String> {
    if !p.is_absolute() {
        return Err("not an absolute path".into());
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != 0 {
            return Err("not owned by root".into());
        }
        if meta.mode() & 0o022 != 0 {
            return Err("writable by group or other".into());
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    Ok(())
}

pub struct Sudo;

impl PrivilegedExec for Sudo {
    fn exec(&self, call: PrivilegedCall) -> PrivilegedFuture {
        Box::pin(async move {
            refuse_elevation(&call)?;
            spawn_sudo(call).await
        })
    }
}

#[cfg(any(test, feature = "test-scope"))]
async fn spawn_sudo(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    Err(testing::refused(&call))
}

#[cfg(not(any(test, feature = "test-scope")))]
async fn spawn_sudo(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    use tokio::io::AsyncWriteExt;

    let sudo = if std::path::Path::new("/usr/bin/sudo").is_file() {
        "/usr/bin/sudo"
    } else {
        "sudo"
    };
    let mut child = tokio::process::Command::new(sudo)
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
    let out = child.wait_with_output().await?;
    Ok(PrivilegedOutput {
        success: out.status.success(),
        code: out.status.code(),
        status: out.status.to_string(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
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
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
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

    #[test]
    fn only_absolute_root_owned_unwritable_programs_are_trusted() {
        assert!(trusted_program(std::path::Path::new("orca")).is_err());
        assert!(trusted_program(std::path::Path::new("/nonexistent/orca")).is_err());
        let f = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o757)).unwrap();
        }
        assert!(trusted_program(f.path()).is_err());
        #[cfg(unix)]
        assert!(trusted_program(std::path::Path::new("/bin/sh")).is_ok());
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
