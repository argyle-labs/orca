use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub use utils::path::expand_tilde;

/// Resolve orca's state dir: `$ORCA_HOME` if set, else `$HOME/.orca`.
/// Returns `None` when neither env var is set (test sandboxes, sealed CI).
///
/// Thin re-export of the canonical resolver in `contract::config::paths` — the
/// single source of truth for orca state-dir resolution. Kept here for the many
/// call sites that already import `files::ops::orca_home`.
pub fn orca_home() -> Option<PathBuf> {
    contract::config::orca_home()
}

/// Restrict a directory to mode 0700 (owner-only rwx). No-op on non-unix.
#[cfg(unix)]
pub fn chmod_dir_owner_only(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(dir)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)
}

#[cfg(not(unix))]
pub fn chmod_dir_owner_only(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Read a file's contents. Returns an error message string on failure (not Err)
/// so the model can see what went wrong.
pub fn read_file(path: &str) -> Result<String> {
    let p = Path::new(path);
    if !p.exists() {
        bail!("file not found: {path}");
    }
    Ok(std::fs::read_to_string(p)?)
}

/// Write content to a file, creating it if it doesn't exist.
pub fn write_file(path: &str, content: &str, allow_unparseable: bool) -> Result<String> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Validated and atomic when the extension names a format we can parse.
    // This path was a bare `fs::write`: it would plant an unparseable config as
    // readily as a good one, and a crash mid-write left half a file. That is
    // the #563 failure — a service up and serving on config it could not read,
    // for six months, while reporting healthy.
    let overridden = utils::config_format::write_guarded(p, content.as_bytes(), allow_unparseable)?;
    if let Some(error) = &overridden {
        tracing::warn!(path, %error, "files.update forced past config validation");
    }
    let mut msg = format!("wrote {} bytes to {path}", content.len());
    if overridden.is_some() {
        // Say it in the RESULT, not only the log. An operator who forced a bad
        // config past the guard should be told they did, where they are looking.
        msg.push_str(" (FORCED past config validation: the file does not parse)");
    }
    Ok(msg)
}

/// `true` iff `path` exists. Symlinks resolve to their target.
pub fn exists(path: &Path) -> bool {
    path.exists()
}

/// Create `path` and all missing ancestors. No-op when already present.
pub fn mkdir_p(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("mkdir -p {}", path.display()))
}

/// Remove `path`. Files are unlinked; directories are removed recursively.
/// Errors when the path doesn't exist.
pub fn remove(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("path not found: {}", path.display());
    }
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Replace the first occurrence of `old` with `new` in the file at `path`.
pub fn edit_file(path: &str, old: &str, new: &str) -> Result<String> {
    let p = Path::new(path);
    if !p.exists() {
        bail!("file not found: {path}");
    }
    let content = std::fs::read_to_string(p)?;
    if !content.contains(old) {
        bail!("old_string not found in {path}");
    }
    let count = content.matches(old).count();
    if count > 1 {
        bail!("old_string matches {count} times in {path} — make it more specific");
    }
    let updated = content.replacen(old, new, 1);
    // An in-place edit of a config is the likeliest way to break one — the
    // jellyfin `network.xml` in #563 was mangled by an over-eager sed, which is
    // exactly this operation. Guarded and atomic; no opt-out here, because an
    // edit presupposes a file that already parsed.
    utils::config_format::write_guarded(p, updated.as_bytes(), false)?;
    Ok(format!("edit applied to {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn edit_file_rejects_missing_string() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "hello world").unwrap();
        let path = f.path().to_str().unwrap().to_string();
        let err = edit_file(&path, "nonexistent", "replacement").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn edit_file_rejects_duplicate_match() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "foo foo").unwrap();
        let path = f.path().to_str().unwrap().to_string();
        let err = edit_file(&path, "foo", "bar").unwrap_err();
        assert!(err.to_string().contains("matches 2"), "got: {err}");
    }

    #[test]
    fn edit_file_applies_single_match() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "hello world").unwrap();
        f.flush().unwrap();
        let path = f.path().to_str().unwrap().to_string();
        edit_file(&path, "world", "rust").unwrap();
        assert_eq!(read_file(&path).unwrap(), "hello rust");
    }

    #[test]
    fn read_file_missing_returns_err() {
        let result = read_file("/tmp/brain_test_nonexistent_xyz_999.txt");
        assert!(result.is_err());
    }

    #[test]
    fn write_file_creates_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt").to_str().unwrap().to_string();
        write_file(&path, "brain content", false).unwrap();
        assert_eq!(read_file(&path).unwrap(), "brain content");
    }

    // ── #563: a service must never be left on config it cannot read ───────

    /// The shape of the file that broke: every XML attribute quote stripped,
    /// as an over-eager sed left `/etc/jellyfin/network.xml` on frigg.
    const MANGLED_XML: &str = "<?xml version=1.0 encoding=utf-8?>\n<Net><A>1</A></Net>";

    #[test]
    fn an_unparseable_config_is_refused_and_the_old_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.xml").to_str().unwrap().to_string();
        write_file(&path, "<Net><A>1</A></Net>", false).expect("good xml");

        let err = write_file(&path, MANGLED_XML, false).expect_err("must refuse");
        assert!(
            err.to_string().contains("allow-unparseable")
                || format!("{err:#}").contains("allow-unparseable"),
            "the error must say how to force it: {err:#}"
        );
        // Refusing is only half of it. The previous good config has to still be
        // there — a guard that rejects the write after truncating the file would
        // leave the service worse off than the bad write would have.
        assert_eq!(read_file(&path).unwrap(), "<Net><A>1</A></Net>");
    }

    #[test]
    fn forcing_an_unparseable_config_works_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.xml").to_str().unwrap().to_string();
        let msg = write_file(&path, MANGLED_XML, true).expect("forced");
        // Reported where the operator is looking, not only in a log they are not
        // reading. Repairing a broken file is legitimate; doing it unknowingly
        // is the six-month failure.
        assert!(msg.contains("FORCED"), "{msg}");
        assert_eq!(read_file(&path).unwrap(), MANGLED_XML);
    }

    #[test]
    fn the_guard_has_no_opinion_about_formats_it_cannot_parse() {
        // A guard that refused opaque bytes would break every PEM, key, and
        // blob write. Unknown extensions pass through untouched.
        let dir = tempfile::tempdir().unwrap();
        for name in ["notes.txt", "key.pem", "blob", "script.sh"] {
            let path = dir.path().join(name).to_str().unwrap().to_string();
            write_file(&path, MANGLED_XML, false)
                .unwrap_or_else(|e| panic!("{name} must not be guarded: {e:#}"));
        }
    }

    #[test]
    fn every_guarded_format_is_actually_checked() {
        let dir = tempfile::tempdir().unwrap();
        // Content that is invalid in all three.
        for (name, bad) in [
            ("a.json", "{\"unclosed\": "),
            ("b.yaml", "a:\n\t- tab indent"),
            ("c.toml", "key = "),
            ("d.xml", MANGLED_XML),
        ] {
            let path = dir.path().join(name).to_str().unwrap().to_string();
            assert!(
                write_file(&path, bad, false).is_err(),
                "{name} must be refused"
            );
        }
    }

    #[test]
    fn an_edit_cannot_break_a_config_that_parsed() {
        // The #563 mechanism exactly: an in-place edit of a working config.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.xml").to_str().unwrap().to_string();
        write_file(&path, "<Net><A>1</A></Net>", false).unwrap();
        let err = edit_file(&path, "<A>1</A>", "<A>1</A").expect_err("broken xml");
        assert!(format!("{err:#}").contains("xml"), "{err:#}");
        // And the file it was editing is untouched.
        assert_eq!(read_file(&path).unwrap(), "<Net><A>1</A></Net>");
    }

    #[test]
    fn a_valid_edit_still_applies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("network.xml").to_str().unwrap().to_string();
        write_file(&path, "<Net><A>1</A></Net>", false).unwrap();
        edit_file(&path, "<A>1</A>", "<A>2</A>").expect("valid edit");
        assert_eq!(read_file(&path).unwrap(), "<Net><A>2</A></Net>");
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("sub/dir/file.txt")
            .to_str()
            .unwrap()
            .to_string();
        write_file(&path, "nested", false).unwrap();
        assert_eq!(read_file(&path).unwrap(), "nested");
    }
}
