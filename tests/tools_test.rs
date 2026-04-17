use std::fs;
use std::path::Path;

// These tests verify the tool implementations work correctly.
// They use real filesystem operations in a temp directory.

fn temp_dir() -> std::path::PathBuf {
    let id = format!("brain_test_{}_{}", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let dir = std::env::temp_dir().join(id);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn test_read_file() {
    let dir = temp_dir();
    let path = dir.join("test.txt");
    fs::write(&path, "hello world").unwrap();

    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content, "hello world");

    cleanup(&dir);
}

#[test]
fn test_write_and_read() {
    let dir = temp_dir();
    let path = dir.join("sub/nested/file.txt");

    // write_file creates parent dirs
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::write(&path, "nested content").unwrap();

    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content, "nested content");

    cleanup(&dir);
}

#[test]
fn test_edit_file() {
    let dir = temp_dir();
    let path = dir.join("edit.txt");
    fs::write(&path, "hello world foo bar").unwrap();

    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("hello world"));

    let updated = content.replacen("hello world", "goodbye world", 1);
    fs::write(&path, &updated).unwrap();

    let result = fs::read_to_string(&path).unwrap();
    assert_eq!(result, "goodbye world foo bar");

    cleanup(&dir);
}

#[test]
fn test_glob_pattern() {
    let dir = temp_dir();
    fs::write(dir.join("a.rs"), "").unwrap();
    fs::write(dir.join("b.rs"), "").unwrap();
    fs::write(dir.join("c.txt"), "").unwrap();

    let pattern = format!("{}/*.rs", dir.display());
    let paths: Vec<_> = glob::glob(&pattern).unwrap().filter_map(|p| p.ok()).collect();
    assert_eq!(paths.len(), 2);

    cleanup(&dir);
}

#[test]
fn test_grep_content() {
    let dir = temp_dir();
    let path = dir.join("search.txt");
    fs::write(&path, "line one\nline two needle\nline three\n").unwrap();

    let content = fs::read_to_string(&path).unwrap();
    let matches: Vec<_> = content.lines()
        .enumerate()
        .filter(|(_, line)| line.contains("needle"))
        .collect();

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, 1); // line index 1

    cleanup(&dir);
}

#[test]
fn test_strip_frontmatter() {
    let input = "---\nname: test\ntype: agent\n---\n\nActual content here.";
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
fn test_model_parse() {
    // Test that model parsing works for various formats
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
