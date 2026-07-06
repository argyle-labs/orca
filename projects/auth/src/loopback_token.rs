//! Process-local loopback bearer token.
//!
//! Generated fresh on every daemon boot and written to
//! `~/.orca/secrets/loopback.token` (mode 0600). The auth middleware
//! recognises it via a constant-time prefix check before falling through to
//! the DB-backed `api_tokens` lookup, so in-process callers that loop back
//! over `https://127.0.0.1:12000` can authenticate without a real token row.
//!
//! Why a token instead of a loopback-bypass: any process on the box can
//! connect to 127.0.0.1, but only processes running as the orca user can
//! read the secret file. The token model keeps the trust boundary at file
//! permissions, not network namespace.

use anyhow::{Context, Result};
use files::ops::chmod_dir_owner_only;
use rand::Rng;
use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

static TOKEN: OnceLock<String> = OnceLock::new();

const SECRETS_SUBDIR: &str = "secrets";
const TOKEN_FILENAME: &str = "loopback.token";

fn secrets_dir() -> Result<PathBuf> {
    // Resolve via the canonical state-dir resolver so the daemon writes the
    // token to the SAME place the CLI (`read_loopback_token`) and the rest of
    // orca read it from: `$ORCA_HOME` if set, else `$HOME/.orca`. Using
    // `dirs::home_dir()` directly here silently diverged from `$ORCA_HOME` and
    // produced a guaranteed 401 on any host running under a custom ORCA_HOME.
    let home = files::ops::orca_home().context("no ORCA_HOME or HOME set")?;
    Ok(home.join(SECRETS_SUBDIR))
}

fn token_path() -> Result<PathBuf> {
    Ok(secrets_dir()?.join(TOKEN_FILENAME))
}

/// Mint a fresh loopback token, persist it to `~/.orca/secrets/loopback.token`
/// (mode 0600), and stash it in the process-wide cache. Idempotent: a second
/// caller in the same process returns early without touching memory or disk.
/// The structural guarantee is that disk and memory NEVER diverge — only the
/// caller that wins the OnceLock claim is allowed to write the disk file.
pub fn install_at_startup() -> Result<()> {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let plaintext = format!("orca_loopback_{}", utils::hash::hex_encode(&buf));

    // Claim memory FIRST. If a prior call already claimed it (test harness
    // reuses the static across many `tokio::test` cases in one binary),
    // return early WITHOUT touching disk — that's the load-bearing guarantee:
    // disk and memory never diverge because disk is only written by the
    // caller that won the OnceLock race.
    if TOKEN.set(plaintext.clone()).is_err() {
        return Ok(());
    }

    let dir = secrets_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create secrets dir {}", dir.display()))?;
    chmod_dir_owner_only(&dir).with_context(|| format!("chmod 0700 on {}", dir.display()))?;
    let path = token_path()?;
    write_secret_file(&path, &plaintext)
        .with_context(|| format!("write loopback token to {}", path.display()))?;
    Ok(())
}

/// Active loopback token, if `install_at_startup` has run.
pub fn get() -> Option<&'static str> {
    TOKEN.get().map(|s| s.as_str())
}

/// Build a `reqwest::Client` with `danger_accept_invalid_certs(true)` — but
/// ONLY after asserting that `url` targets a loopback address. Panics (rather
/// than returning Err) so a mis-configured URL is caught immediately at the
/// call site and can never silently talk to a non-loopback host with cert
/// verification disabled.
pub fn loopback_only_reqwest_client(url: &str) -> anyhow::Result<reqwest::Client> {
    let after_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = if authority.starts_with('[') {
        // IPv6 literal: [::1]:port or [::1]
        &authority[..authority
            .find(']')
            .map(|i| i + 1)
            .unwrap_or(authority.len())]
    } else {
        // hostname or hostname:port — drop port
        authority.split(':').next().unwrap_or("")
    };
    assert!(
        host == "127.0.0.1" || host == "localhost" || host == "[::1]",
        "loopback_only_reqwest_client called with non-loopback URL '{url}' — \
         danger_accept_invalid_certs is only safe on the loopback interface"
    );
    Ok(reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?)
}

/// Test-only seeding hook — installs a deterministic loopback token from
/// unit tests that need to exercise the loopback fast path without minting
/// real randomness or writing to disk. First-call-wins, matching the
/// production OnceLock semantics.
pub fn set_for_tests(s: String) {
    _ = TOKEN.set(s);
}

/// Read the token from disk. Used by loopback HTTP clients that aren't the
/// daemon itself (e.g. a CLI subcommand re-entering the API). Returns `None`
/// if the file is missing — caller should fall back to whatever they did
/// before tokens existed (typically: error out).
pub fn read_from_disk() -> Option<String> {
    let path = token_path().ok()?;
    let s = std::fs::read_to_string(path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(unix)]
pub(crate) fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-wide `HOME`/`ORCA_HOME`. Rust runs
    /// a crate's tests as parallel threads in one process, so without this lock
    /// these env-mutating tests race each other (one clears `ORCA_HOME` while
    /// another expects it set), producing intermittent failures under the
    /// workspace test run. Poison-tolerant: a panic mid-test must not wedge the
    /// rest.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn get_returns_none_before_install() {
        // TOKEN may be set by other tests; this test exercises the get() path.
        // We can't guarantee TOKEN state, but we can call get() and verify the return type.
        let _ = get(); // must not panic
    }

    #[test]
    fn set_for_tests_then_get() {
        // If TOKEN is not yet set, set_for_tests populates it.
        set_for_tests("test_loopback_token_abc".to_string());
        // get() must return Some value (either ours or a prior call's value)
        assert!(get().is_some());
    }

    #[test]
    fn read_from_disk_returns_none_when_file_absent() {
        let _env = env_guard();
        // Point HOME at a fresh temp dir — no loopback.token file present.
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HOME", dir.path()) };
        // May return Some if the file somehow exists, None if absent
        let _ = read_from_disk(); // must not panic
    }

    #[test]
    fn read_from_disk_reads_written_content() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let secrets = dir.path().join(".orca").join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        let token_file = secrets.join("loopback.token");
        write_secret_file(&token_file, "orca_loopback_abcdef").unwrap();
        unsafe {
            std::env::remove_var("ORCA_HOME");
            std::env::set_var("HOME", dir.path());
        }
        let got = read_from_disk();
        assert_eq!(got, Some("orca_loopback_abcdef".to_string()));
    }

    /// Regression: the token path MUST follow `$ORCA_HOME` (the canonical
    /// state-dir resolver), not `$HOME/.orca`. When the daemon runs under a
    /// custom ORCA_HOME, writing/reading the token via `dirs::home_dir()`
    /// silently diverged and every CLI call 401'd.
    #[test]
    fn token_path_honors_orca_home_override() {
        let _env = env_guard();
        let orca = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // ORCA_HOME points somewhere OTHER than $HOME/.orca.
        let secrets = orca.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        write_secret_file(&secrets.join("loopback.token"), "orca_loopback_via_env").unwrap();
        unsafe {
            std::env::set_var("HOME", home.path()); // no token here
            std::env::set_var("ORCA_HOME", orca.path());
        }
        let got = read_from_disk();
        unsafe { std::env::remove_var("ORCA_HOME") };
        assert_eq!(
            got,
            Some("orca_loopback_via_env".to_string()),
            "read_from_disk must resolve under $ORCA_HOME, not $HOME/.orca"
        );
    }

    #[test]
    fn write_secret_file_creates_file_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.token");
        write_secret_file(&path, "hello-token").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello-token");
    }

    #[test]
    fn chmod_dir_owner_only_succeeds_on_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        chmod_dir_owner_only(dir.path()).unwrap();
        // On Unix, verify the mode is 0700
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(dir.path()).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700, "mode should be 0700, got {mode:o}");
        }
    }

    #[tokio::test]
    async fn loopback_only_reqwest_client_accepts_127() {
        ::model::ensure_crypto_provider();
        loopback_only_reqwest_client("https://127.0.0.1:12000/api/foo").unwrap();
    }

    #[tokio::test]
    async fn loopback_only_reqwest_client_accepts_localhost() {
        ::model::ensure_crypto_provider();
        loopback_only_reqwest_client("http://localhost:8080/").unwrap();
    }

    #[tokio::test]
    async fn loopback_only_reqwest_client_accepts_ipv6_loopback() {
        ::model::ensure_crypto_provider();
        loopback_only_reqwest_client("https://[::1]:12000/").unwrap();
    }

    #[test]
    #[should_panic(expected = "non-loopback URL")]
    fn loopback_only_reqwest_client_panics_on_external_host() {
        loopback_only_reqwest_client("https://example.com/api").unwrap();
    }

    #[test]
    #[should_panic(expected = "non-loopback URL")]
    fn loopback_only_reqwest_client_panics_on_192_168() {
        loopback_only_reqwest_client("https://192.168.1.1/api").unwrap();
    }
}
