//! Atomic file writes — the one place in the workspace that knows how orca
//! writes a file crash-safely. Contents land in a sibling temp file that is
//! fsynced, then `rename`d into place, so a concurrent reader sees either the
//! old contents or the new contents, never a half-written file.
//!
//! Lives in `utils` (the leaf crate) on purpose: even `utils::state` needs it,
//! and `utils` cannot depend on the `files` crate (which depends on `utils`).
//! An earlier `files::atomic` had the same shape but sat above the leaf and had
//! no callers — this replaces it. Async writers (e.g. `system::autofs`, on
//! `tokio::fs`) keep their own variant; this is the synchronous primitive.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::Path;

/// Write `contents` to `path` atomically: temp file in the same directory,
/// fsync, then rename over `path`. The parent directory must already exist
/// (use [`write_mkdir`] otherwise).
pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if !parent.exists() {
        anyhow::bail!("parent directory does not exist: {}", parent.display());
    }
    // Temp lives in the same dir so `persist` is a same-filesystem rename.
    let mut tmp = tempfile::Builder::new()
        .prefix(".orca-tmp-")
        .tempfile_in(parent)
        .with_context(|| format!("create temp file in {}", parent.display()))?;
    tmp.write_all(contents)
        .with_context(|| format!("write temp for {}", path.display()))?;
    // fsync the contents before the rename so a crash can't leave a renamed
    // but empty file (the durability guarantee the hand-rolled sites relied on).
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("fsync temp for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow!("persist temp file to {}: {e}", path.display()))?;
    Ok(())
}

/// Like [`write`], creating any missing parent directories first.
pub fn write_mkdir(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent {}", parent.display()))?;
    }
    write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        write(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn write_overwrites_existing_atomically() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        write(&p, b"first").unwrap();
        write(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn write_errors_when_parent_missing() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nope").join("a.txt");
        assert!(write(&p, b"x").is_err());
    }

    #[test]
    fn write_mkdir_creates_parents() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("deep").join("nested").join("a.txt");
        write_mkdir(&p, b"x").unwrap();
        assert!(p.exists());
    }
}
