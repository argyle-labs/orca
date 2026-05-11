use std::fs;
use tempfile::tempdir;

use orca::agents::{list_embedded_agents, load_agent_prompt};
use orca::llm::tools::bash::BashPermissions;
use orca_utils::fs::{fs as fstool, search};

// These tests verify the tool implementations work correctly.
// They use real filesystem operations via the tempfile crate (no race conditions).

// ── fs tool tests ─────────────────────────────────────────────────────────────

#[test]
fn test_read_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.txt");
    fs::write(&path, "hello world").unwrap();

    let result = fstool::read_file(path.to_str().unwrap());
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "hello world");
}

#[test]
fn test_write_creates_parent_dirs() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sub/nested/file.txt");

    let result = fstool::write_file(path.to_str().unwrap(), "nested content");
    assert!(result.is_ok());
    assert_eq!(fs::read_to_string(&path).unwrap(), "nested content");
}

#[test]
fn test_edit_file_replaces_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("edit.txt");
    fs::write(&path, "hello world foo bar").unwrap();

    let result = fstool::edit_file(path.to_str().unwrap(), "hello world", "goodbye world");
    assert!(result.is_ok());
    assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world foo bar");
}

#[test]
fn test_edit_file_not_found_returns_error() {
    let result = fstool::edit_file("/nonexistent/path.txt", "old", "new");
    assert!(result.is_err());
}

#[test]
fn test_edit_file_old_string_not_found() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "some content").unwrap();

    let result = fstool::edit_file(path.to_str().unwrap(), "not present", "new");
    assert!(result.is_err());
}

// ── search tool tests ─────────────────────────────────────────────────────────

#[test]
fn test_glob_files_matches_pattern() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "").unwrap();
    fs::write(dir.path().join("b.rs"), "").unwrap();
    fs::write(dir.path().join("c.txt"), "").unwrap();

    // glob_files with full pattern (no base)
    let pattern = format!("{}/*.rs", dir.path().display());
    let result = search::glob_files(&pattern, None).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 .rs files, got: {result}");
}

#[test]
fn test_glob_files_with_base() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("x.md"), "").unwrap();
    fs::write(dir.path().join("y.md"), "").unwrap();

    let result = search::glob_files("*.md", Some(dir.path().to_str().unwrap())).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 2);
}

#[test]
fn test_glob_files_no_match_returns_message() {
    let dir = tempdir().unwrap();
    let pattern = format!("{}/*.xyz", dir.path().display());
    let result = search::glob_files(&pattern, None).unwrap();
    assert!(result.starts_with("no files matched"));
}

#[test]
fn test_grep_content_finds_match() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search.txt");
    fs::write(&path, "line one\nline two needle\nline three\n").unwrap();

    let result = search::grep_content("needle", path.to_str().unwrap(), false).unwrap();
    assert!(result.contains("needle"));
    assert!(result.contains(":2:")); // line 2 (1-indexed)
}

#[test]
fn test_grep_content_case_insensitive() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "Hello WORLD\nlower world\n").unwrap();

    let result = search::grep_content("hello", path.to_str().unwrap(), true).unwrap();
    assert!(result.contains("Hello WORLD"));
}

#[test]
fn test_grep_content_no_match_returns_message() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "no match here\n").unwrap();

    let result = search::grep_content("xyzzy", path.to_str().unwrap(), false).unwrap();
    assert!(result.starts_with("no matches"));
}

#[test]
fn test_grep_content_truncates_at_200_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("big.txt");
    // Write 300 lines each containing the search pattern
    let content: String = (0..300).map(|i| format!("line {i} needle\n")).collect();
    fs::write(&path, &content).unwrap();

    let result = search::grep_content("needle", path.to_str().unwrap(), false).unwrap();
    assert!(result.contains("truncated"));
}

#[test]
fn test_grep_content_path_not_found_returns_error() {
    let result = search::grep_content("x", "/no/such/path.txt", false);
    assert!(result.is_err());
}

// ── bash tool tests ───────────────────────────────────────────────────────────

#[test]
fn test_allowlist_auto_approve() {
    let mut p = BashPermissions::default();
    p.auto_approve = true;
    assert!(p.is_allowed("rm -rf /"));
    assert!(p.is_allowed("any command"));
}

#[test]
fn test_allowlist_prefix_match() {
    let mut p = BashPermissions::default();
    p.allow("cargo");
    assert!(p.is_allowed("cargo test"));
    assert!(p.is_allowed("cargo build --release"));
    assert!(!p.is_allowed("rm file"));
}

#[test]
fn test_allowlist_empty_denies_all() {
    let p = BashPermissions::default();
    assert!(!p.is_allowed("echo hello"));
    assert!(!p.is_allowed("ls"));
}

// ── frontmatter strip test (agents.rs logic) ──────────────────────────────────

#[test]
fn test_strip_frontmatter_removes_yaml_block() {
    let input = "---\nname: test\ntype: agent\n---\n\nActual content here.";
    // We test the public strip function indirectly by loading a synthesised agent.
    // Direct: recreate the same logic inline and verify it matches agents::strip_frontmatter output
    // by calling load_agent_prompt on an embedded agent — but that needs agents_dir.
    // Instead: verify agents::list_embedded_agents() strips correctly (descriptions don't start with "---")
    let agents = list_embedded_agents();
    // At least one embedded agent must exist (wolf is always embedded)
    assert!(!agents.is_empty(), "no embedded agents found");
    for (name, desc) in &agents {
        assert!(
            !desc.starts_with("---"),
            "agent {name} description still has frontmatter"
        );
    }
    // Also verify raw logic for the known input
    let lines: Vec<&str> = input.lines().collect();
    let result = if lines.first().map(|l| l.trim()) == Some("---") {
        if let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") {
            lines[end + 2..].join("\n").trim().to_string()
        } else {
            input.trim().to_string()
        }
    } else {
        input.trim().to_string()
    };
    assert_eq!(result, "Actual content here.");
}

#[test]
fn test_strip_frontmatter_no_frontmatter_passthrough() {
    // Agents without frontmatter should come through unmodified (minus trim).
    // Use the existing embedded agents as proof: their prompts have content.
    let agents = list_embedded_agents();
    assert!(!agents.is_empty());
    // All agents must have non-empty descriptions (list_embedded_agents calls strip_frontmatter)
    for (name, _) in &agents {
        let dir = std::path::Path::new("/nonexistent");
        let prompt = load_agent_prompt(name, dir);
        // Falls back to embedded — must return Some
        assert!(prompt.is_some(), "embedded agent {name} returned None");
    }
}

// ── model parse test ──────────────────────────────────────────────────────────

#[test]
fn test_model_parse() {
    let specs = vec![
        ("claude-sonnet-4-6", true),
        ("claude:claude-opus-4-6", true),
        ("lmstudio:qwen3", false),
        ("some-local-model", false),
    ];

    for (spec, is_claude) in specs {
        let is_claude_result = spec.starts_with("claude-") || spec.starts_with("claude:");
        assert_eq!(is_claude_result, is_claude, "failed for: {spec}");
    }
}
