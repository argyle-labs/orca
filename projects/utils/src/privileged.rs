use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedCall {
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Option<Duration>,
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

impl PrivilegedCall {
    pub fn new<I, S>(argv: I, stdin: impl Into<Vec<u8>>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            stdin: stdin.into(),
            timeout: None,
        }
    }

    pub fn helper(subcommand: &str, stdin: impl Into<Vec<u8>>) -> std::io::Result<Self> {
        let exe = crate::path::privileged_exe()?;
        Ok(Self::new(
            [
                exe.to_string_lossy().into_owned(),
                "admin".into(),
                subcommand.into(),
            ],
            stdin,
        ))
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

pub async fn run(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    #[cfg(any(test, feature = "test-scope"))]
    {
        testing::dispatch(call).await
    }
    #[cfg(not(any(test, feature = "test-scope")))]
    {
        Sudo.exec(call).await
    }
}

pub struct Sudo;

impl PrivilegedExec for Sudo {
    fn exec(&self, call: PrivilegedCall) -> PrivilegedFuture {
        #[cfg(any(test, feature = "test-scope"))]
        {
            Box::pin(async move { Err(testing::refused(&call)) })
        }
        #[cfg(not(any(test, feature = "test-scope")))]
        {
            Box::pin(sudo(call))
        }
    }
}

#[cfg(not(any(test, feature = "test-scope")))]
async fn sudo(call: PrivilegedCall) -> std::io::Result<PrivilegedOutput> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new("sudo")
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
    let out = match call.timeout {
        Some(t) => tokio::time::timeout(t, child.wait_with_output())
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("privileged helper did not finish within {}s", t.as_secs()),
                )
            })??,
        None => child.wait_with_output().await?,
    };
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
        let err = run(PrivilegedCall::new(["/bin/true"], Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_real_sudo_executor_is_a_hard_error_in_test_builds() {
        let err = Sudo
            .exec(PrivilegedCall::new(["/bin/true"], Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
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
            run(PrivilegedCall::new(["orca", "admin", "x"], b"in".to_vec())),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, b"ok");
        assert_eq!(
            rec.calls(),
            vec![PrivilegedCall::new(["orca", "admin", "x"], b"in".to_vec())]
        );
    }
}
