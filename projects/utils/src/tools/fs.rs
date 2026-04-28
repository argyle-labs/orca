use anyhow::{Result, bail};
use std::path::Path;

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
pub fn write_file(path: &str, content: &str) -> Result<String> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, content)?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
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
    std::fs::write(p, &updated)?;
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
        write_file(&path, "brain content").unwrap();
        assert_eq!(read_file(&path).unwrap(), "brain content");
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
        write_file(&path, "nested").unwrap();
        assert_eq!(read_file(&path).unwrap(), "nested");
    }
}
