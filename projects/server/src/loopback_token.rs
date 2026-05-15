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
fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
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
fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}
