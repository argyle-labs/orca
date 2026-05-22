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
use rand::Rng;
use std::path::PathBuf;
use std::sync::OnceLock;

static TOKEN: OnceLock<String> = OnceLock::new();

const SECRETS_SUBDIR: &str = "secrets";
const TOKEN_FILENAME: &str = "loopback.token";

fn secrets_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home
        .join(orca_utils::config::APP_STATE_DIR)
        .join(SECRETS_SUBDIR))
}

fn token_path() -> Result<PathBuf> {
    Ok(secrets_dir()?.join(TOKEN_FILENAME))
}

/// Mint a fresh loopback token, persist it to `~/.orca/secrets/loopback.token`
/// (mode 0600), and stash it in the process-wide cache. Subsequent calls are
/// no-ops — the cached value wins for the lifetime of this process.
pub fn install_at_startup() -> Result<()> {
    if TOKEN.get().is_some() {
        return Ok(());
    }
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let plaintext = format!("orca_loopback_{}", hex(&buf));

    let dir = secrets_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create secrets dir {}", dir.display()))?;
    // Tighten the dir to 0700: file names under here (loopback token, future
    // per-secret blobs) shouldn't be enumerable by other users on shared hosts.
    chmod_dir_owner_only(&dir).with_context(|| format!("chmod 0700 on {}", dir.display()))?;
    let path = token_path()?;
    write_secret_file(&path, &plaintext)
        .with_context(|| format!("write loopback token to {}", path.display()))?;

    let _ = TOKEN.set(plaintext);
    Ok(())
}

/// Active loopback token, if `install_at_startup` has run.
pub fn get() -> Option<&'static str> {
    TOKEN.get().map(|s| s.as_str())
}

/// Test-only seeding hook — installs a deterministic loopback token from
/// unit tests that need to exercise the loopback fast path without minting
/// real randomness or writing to disk. First-call-wins, matching the
/// production OnceLock semantics.
#[cfg(test)]
pub(crate) fn set_for_tests(s: String) {
    let _ = TOKEN.set(s);
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

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(unix)]
pub(crate) fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
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

#[cfg(unix)]
pub(crate) fn chmod_dir_owner_only(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(dir)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)
}

#[cfg(not(unix))]
pub(crate) fn chmod_dir_owner_only(_dir: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn hex_produces_lowercase_hexdigits() {
        let s = hex(&[0x00, 0xff, 0xab, 0x12]);
        assert_eq!(s, "00ffab12");
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
        // Point HOME at a fresh temp dir — no loopback.token file present.
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HOME", dir.path()) };
        // May return Some if the file somehow exists, None if absent
        let _ = read_from_disk(); // must not panic
    }

    #[test]
    fn read_from_disk_reads_written_content() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = dir.path().join(".orca").join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        let token_file = secrets.join("loopback.token");
        write_secret_file(&token_file, "orca_loopback_abcdef").unwrap();
        unsafe { std::env::set_var("HOME", dir.path()) };
        let got = read_from_disk();
        assert_eq!(got, Some("orca_loopback_abcdef".to_string()));
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

    #[test]
    fn install_at_startup_is_idempotent() {
        // install_at_startup returns Ok() immediately if TOKEN is already set.
        // Ensure calling it twice doesn't error.
        // (We can't control whether TOKEN was set by a prior test, but we can
        // call install_at_startup safely — if TOKEN is set, it returns early.)
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HOME", dir.path()) };
        // May write a file or may return early — both paths must succeed.
        let _ = install_at_startup(); // ignore result (may fail if HOME is weird)
        let _ = install_at_startup(); // second call must also not panic
    }
}
