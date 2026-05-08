use anyhow::{Result, bail};
use std::path::Path;

/// Find files matching a glob pattern under an optional base directory.
pub fn glob_files(pattern: &str, base: Option<&str>) -> Result<String> {
    let full_pattern = match base {
        Some(b) => format!("{b}/{pattern}"),
        None => pattern.to_string(),
    };

    let paths: Vec<String> = glob::glob(&full_pattern)
        .map_err(|e| anyhow::anyhow!("invalid glob pattern: {e}"))?
        .filter_map(|entry| entry.ok())
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    if paths.is_empty() {
        return Ok(format!("no files matched: {full_pattern}"));
    }

    Ok(paths.join("\n"))
}

/// Search file contents for a pattern (literal string, not regex).
/// Returns matching lines with file:line format.
pub fn grep_content(pattern: &str, path: &str, case_insensitive: bool) -> Result<String> {
    let p = Path::new(path);
    if !p.exists() {
        bail!("path not found: {path}");
    }

    let mut results: Vec<String> = Vec::new();

    if p.is_file() {
        search_file(p, pattern, case_insensitive, &mut results)?;
    } else if p.is_dir() {
        search_dir(p, pattern, case_insensitive, &mut results)?;
    }

    if results.is_empty() {
        return Ok(format!("no matches for '{pattern}' in {path}"));
    }

    // Limit output to 200 lines to avoid context explosion
    let total = results.len();
    results.truncate(200);
    let mut out = results.join("\n");
    if total > 200 {
        out.push_str(&format!("\n... ({} more lines truncated)", total - 200));
    }
    Ok(out)
}

fn search_file(
    path: &Path,
    pattern: &str,
    case_insensitive: bool,
    results: &mut Vec<String>,
) -> Result<()> {
    // Skip binary-looking files and very large files
    let meta = std::fs::metadata(path)?;
    if meta.len() > 10_000_000 {
        return Ok(());
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // skip binary files
    };

    let search_pattern = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };

    for (line_no, line) in content.lines().enumerate() {
        let haystack = if case_insensitive {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        if haystack.contains(&search_pattern) {
            results.push(format!("{}:{}: {}", path.display(), line_no + 1, line));
        }
    }

    Ok(())
}

fn search_dir(
    dir: &Path,
    pattern: &str,
    case_insensitive: bool,
    results: &mut Vec<String>,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        // Skip hidden dirs and common noise dirs
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_file() {
            search_file(&path, pattern, case_insensitive, results)?;
        } else if path.is_dir() {
            search_dir(&path, pattern, case_insensitive, results)?;
        }
        if results.len() > 500 {
            break; // safety cap
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    // ── glob_files ────────────────────────────────────────────────────────────

    #[test]
    fn glob_files_finds_matching_files() {
        let dir = tempdir().unwrap();
        write(dir.path(), "foo.txt", "hello");
        write(dir.path(), "bar.txt", "world");
        write(dir.path(), "baz.rs", "rust");

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("foo.txt"), "got: {result}");
        assert!(result.contains("bar.txt"), "got: {result}");
        assert!(!result.contains("baz.rs"), "should not match .rs: {result}");
    }

    #[test]
    fn glob_files_no_match_returns_message() {
        let dir = tempdir().unwrap();
        let result = glob_files("*.nonexistent", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("no files matched"), "got: {result}");
    }

    #[test]
    fn glob_files_invalid_pattern_returns_err() {
        let result = glob_files("[invalid", None);
        assert!(result.is_err(), "invalid glob should error");
    }

    #[test]
    fn glob_files_without_base_uses_absolute_pattern() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "myfile.txt", "hi");
        let pattern = format!("{}/*.txt", dir.path().to_str().unwrap());
        let result = glob_files(&pattern, None).unwrap();
        assert!(
            result.contains(path.file_name().unwrap().to_str().unwrap()),
            "got: {result}"
        );
    }

    // ── grep_content ──────────────────────────────────────────────────────────

    #[test]
    fn grep_content_finds_match_in_file() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.txt", "hello world\nanother line\n");
        let result = grep_content("hello", p.to_str().unwrap(), false).unwrap();
        assert!(result.contains("hello world"), "got: {result}");
    }

    #[test]
    fn grep_content_no_match_returns_message() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.txt", "nothing interesting");
        let result = grep_content("ZZZMISSING", p.to_str().unwrap(), false).unwrap();
        assert!(result.contains("no matches"), "got: {result}");
    }

    #[test]
    fn grep_content_case_insensitive() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.txt", "Hello World");
        let result = grep_content("hello", p.to_str().unwrap(), true).unwrap();
        assert!(result.contains("Hello World"), "got: {result}");
        // Case-sensitive should NOT match
        let result2 = grep_content("hello", p.to_str().unwrap(), false).unwrap();
        assert!(
            result2.contains("no matches"),
            "case-sensitive should miss: {result2}"
        );
    }

    #[test]
    fn grep_content_missing_path_returns_err() {
        let result = grep_content("pattern", "/tmp/__no_such_file_xyz__.txt", false);
        assert!(result.is_err(), "missing path should error");
    }

    #[test]
    fn grep_content_searches_dir_recursively() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("deep.txt"), "needle in haystack").unwrap();
        write(dir.path(), "top.txt", "nothing here");

        let result = grep_content("needle", dir.path().to_str().unwrap(), false).unwrap();
        assert!(
            result.contains("needle"),
            "should find match in subdir: {result}"
        );
    }

    #[test]
    fn grep_content_includes_line_numbers() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.txt", "line one\nfind me\nline three\n");
        let result = grep_content("find me", p.to_str().unwrap(), false).unwrap();
        assert!(
            result.contains(":2:"),
            "should include line number: {result}"
        );
    }
}
