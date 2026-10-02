//! A REAL backup/restore round-trip against a live docker container.
//!
//! Every other test of this path asserts argv and ordering against a fake CLI.
//! Those catch the silent failures well — a wrong archive name, a missing
//! trailing `/.`, an unquiesced write — but none of them moves a byte, so none
//! of them is evidence that a backup can actually be taken and read back.
//!
//! This one does: it writes a file into a container, backs it up, CORRUPTS the
//! file, restores, and checks the original content came back. If the restore
//! silently did nothing, the corrupted content survives and this fails — which
//! is precisely the failure mode #676 describes, where the command succeeds
//! and nothing is restored.
//!
//! Skipped (not failed) when no docker daemon is reachable, so CI and any
//! machine without one stay green. A skip prints why: a test that silently
//! passes by not running is worse than no test.

use std::process::Command;

/// Shell out, returning `(ok, stdout)`.
fn sh(args: &[&str]) -> (bool, String) {
    match Command::new(args[0]).args(&args[1..]).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(_) => (false, String::new()),
    }
}

fn docker_available() -> bool {
    sh(&["docker", "info", "--format", "{{.ServerVersion}}"]).0
}

#[tokio::test]
async fn a_restore_actually_brings_back_the_backed_up_bytes() {
    if !docker_available() {
        eprintln!(
            "SKIP: no reachable docker daemon. This is the only test that proves \
             a real backup can be read back; run it with a daemon up."
        );
        return;
    }

    let name = format!("orca-restore-it-{}", std::process::id());
    let _ = sh(&["docker", "rm", "-f", &name]);

    // A container that stays up and has a shell. `sleep` keeps PID 1 alive so
    // stop/start is meaningful rather than a no-op on an already-exited unit.
    let (ok, err) = sh(&[
        "docker", "run", "-d", "--name", &name, "alpine:3", "sleep", "600",
    ]);
    assert!(ok, "could not start the fixture container: {err}");

    // Cleanup runs on every exit path. A leaked fixture container would make
    // the NEXT run of this test fail for an unrelated reason.
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = sh(&["docker", "rm", "-f", &self.0]);
        }
    }
    let _guard = Cleanup(name.clone());

    {
        assert!(
            sh(&[
                "docker",
                "exec",
                &name,
                "sh",
                "-c",
                "mkdir -p /data && printf ORIGINAL > /data/state.txt",
            ])
            .0,
            "could not seed the fixture data"
        );

        let method = service::backup_method("tar").expect("tar method registered");
        let paths = ["/data".to_string()];
        let ctx = || service::BackupContext {
            runtime: service::Runtime::Docker,
            instance: &name,
            provider: "orca-it",
            data_paths: &paths,
            exclude: &[],
        };

        let artifact = method.backup(ctx()).await.expect("backup");
        assert!(
            std::path::Path::new(&artifact.path).exists(),
            "backup reported `{}` but nothing is there",
            artifact.path
        );

        // Corrupt it. Without this the test would pass even if restore were a
        // no-op, which is the exact bug being guarded against.
        assert!(
            sh(&[
                "docker",
                "exec",
                &name,
                "sh",
                "-c",
                "printf CORRUPTED > /data/state.txt",
            ])
            .0
        );
        assert_eq!(
            sh(&["docker", "exec", &name, "cat", "/data/state.txt"]).1,
            "CORRUPTED",
            "the fixture did not actually change"
        );

        method.restore(ctx(), &artifact).await.expect("restore");

        // The bytes came back...
        assert_eq!(
            sh(&["docker", "exec", &name, "cat", "/data/state.txt"]).1,
            "ORIGINAL",
            "restore reported success but the data did not come back"
        );
        // ...and the unit is RUNNING again. A restore that leaves the service
        // stopped has traded one outage for another.
        assert_eq!(
            sh(&["docker", "inspect", "-f", "{{.State.Running}}", &name]).1,
            "true",
            "the unit was not restarted after the restore"
        );

        drop(std::fs::remove_file(&artifact.path));
    }
}
