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
