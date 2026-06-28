//! Live docker adapter verification. Ignored by default (needs a reachable
//! Docker engine + a running `orca-verify` container). Run explicitly:
//!   docker run -d --name orca-verify alpine sh -c 'echo hi; sleep 60'
//!   cargo test -p containers --test docker_live -- --ignored --nocapture
use containers::adapters::docker::DockerAdapter;
use containers::{LogTail, RuntimeAdapter};

#[tokio::test]
#[ignore]
async fn logs_and_exec_against_live_colima() {
    let a = DockerAdapter::new();

    let logs = a.logs("orca-verify", LogTail(50)).await.expect("logs ok");
    println!("--- logs ---\n{logs}");
    assert!(logs.contains("HELLO_FROM_STDOUT"), "stdout in logs");
    assert!(logs.contains("OOPS_STDERR"), "stderr in logs");

    let out = a
        .exec(
            "orca-verify",
            &[
                "sh".into(),
                "-c".into(),
                "echo exec-stdout; echo exec-stderr 1>&2; exit 7".into(),
            ],
            None,
        )
        .await
        .expect("exec ok");
    println!(
        "--- exec --- code={:?}\nstdout={}\nstderr={}",
        out.exit_code, out.stdout, out.stderr
    );
    assert!(out.stdout.contains("exec-stdout"), "stdout captured");
    assert!(out.stderr.contains("exec-stderr"), "stderr captured");
    assert_eq!(out.exit_code, Some(7), "exit code propagated");

    // stdin path
    let out2 = a
        .exec("orca-verify", &["cat".into()], Some("piped-in\n".into()))
        .await
        .expect("exec stdin ok");
    assert!(out2.stdout.contains("piped-in"), "stdin echoed via cat");
}
