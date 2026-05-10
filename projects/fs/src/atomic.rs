//! Atomic write helpers. Writes happen to a sibling temp file then `rename`
//! into place — readers either see the old contents or the new contents,
//! never a half-written file. Uses `tempfile::NamedTempFile` so the temp
//! sticks around long enough to flush before the rename.

use anyhow::{Context, Result};
use std::path::Path;

/// Write `contents` to `path` atomically. The destination's parent must
/// exist (or use [`write_atomic_mkdir`]).
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    if !parent.as_os_str().is_empty() && !parent.exists() {
        anyhow::bail!("parent directory does not exist: {}", parent.display());
    }
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let tmp = tempfile::Builder::new()
        .prefix(".orca-tmp-")
        .tempfile_in(dir)?;
    std::fs::write(tmp.path(), contents)?;
    // persist() does an atomic rename when target+temp share the same fs.
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("persist temp file: {e}"))?;
    Ok(())
}

/// Write atomically, creating any missing parent directories first.
pub fn write_atomic_mkdir(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_creates_target() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        write_atomic(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"old").unwrap();
        write_atomic(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_rejects_missing_parent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nonexistent/x.txt");
        let err = write_atomic(&p, b"data").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn atomic_write_mkdir_creates_parents() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a/b/c.txt");
        write_atomic_mkdir(&p, b"deep").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"deep");
    }
}
