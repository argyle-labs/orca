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
    let entries = std::fs::read_dir(dir)?;
    for entry in entries.flatten() {
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
