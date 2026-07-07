fn truncate_preview(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

pub fn history_file() -> Option<std::path::PathBuf> {
    // Canonical resolver (honors $ORCA_HOME); was dirs::home_dir().join(".orca").
    let dir = contract::config::orca_home()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("history"))
}

pub fn check_git_changes(dir: &str) -> Option<usize> {
    let output = std::process::Command::new("git")
        .args(["-C", dir, "status", "--short"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    if count == 0 { None } else { Some(count) }
}

pub fn agent_emoji(name: &str) -> &'static str {
    match name {
        "orca" => "☯",
        "wolf" => "🐺",
        "otter" => "🦦",
        "owl" => "🦉",
        "fox" => "🦊",
        "crow" => "🐦‍⬛",
        "bear" => "🐻",
        "spider" => "🕷️",
        "badger" => "🦡",
        "ferret" => "🐾",
        "hawk" => "🦅",
        "mole" => "🐀",
        "elephant" => "🐘",
        "raven" => "🪶",
        "lynx" => "🐱",
        "boar" => "🐗",
        "magpie" => "🐦",
        "oracle" => "🔮",
        _ => "🔧",
    }
}

pub fn find_other_orca_pids() -> Vec<u32> {
    let my_pid = std::process::id();
    let output = std::process::Command::new("pgrep")
        .args(["-x", "orca"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .filter(|&pid| pid != my_pid)
            .collect(),
        _ => vec![],
    }
}

/// Produce a terminal-friendly summary of a tool result.
pub fn summarize_result(tool: &str, content: &str, is_error: bool) -> String {
    if is_error {
        return truncate_preview(content, 300);
    }
    match tool {
        "glob" => {
            let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
            if lines.is_empty() {
                return "(no matches)".to_string();
            }
            format!("{} file(s) matched", lines.len())
        }
        "grep" => {
            let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
            if lines.is_empty() {
                return "(no matches)".to_string();
            }
            let files: std::collections::HashSet<&str> =
                lines.iter().filter_map(|l| l.split(':').next()).collect();
            format!("{} match(es) in {} file(s)", lines.len(), files.len())
        }
        "read_file" => {
            let lines = content.lines().count();
            format!("{lines} lines")
        }
        "write_file" => content.to_string(),
        "edit_file" => content.to_string(),
        "bash" => {
            let mut non_empty = content.lines().filter(|l| !l.trim().is_empty());
            match non_empty.next() {
                None => "(no output)".to_string(),
                Some(first) => {
                    let rest = non_empty.count();
                    if rest == 0 {
                        truncate_preview(first, 120)
                    } else {
                        format!("{} (+{rest} more lines)", truncate_preview(first, 80))
                    }
                }
            }
        }
        _ => truncate_preview(content, 200),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── agent_emoji ───────────────────────────────────────────────────────────

    #[test]
    fn agent_emoji_known_agents() {
        assert_eq!(agent_emoji("wolf"), "🐺");
        assert_eq!(agent_emoji("orca"), "☯");
        assert_eq!(agent_emoji("otter"), "🦦");
        assert_eq!(agent_emoji("owl"), "🦉");
        assert_eq!(agent_emoji("fox"), "🦊");
        assert_eq!(agent_emoji("bear"), "🐻");
        assert_eq!(agent_emoji("oracle"), "🔮");
    }

    #[test]
    fn agent_emoji_unknown_returns_wrench() {
        assert_eq!(agent_emoji("unknown-agent"), "🔧");
        assert_eq!(agent_emoji(""), "🔧");
    }

    // ── truncate_preview ──────────────────────────────────────────────────────

    #[test]
    fn truncate_preview_short_unchanged() {
        assert_eq!(truncate_preview("hello", 10), "hello");
    }

    #[test]
    fn truncate_preview_long_gets_ellipsis() {
        let result = truncate_preview("abcdefghij", 5);
        assert_eq!(result, "abcde…");
    }

    #[test]
    fn truncate_preview_exactly_at_limit_no_ellipsis() {
        assert_eq!(truncate_preview("12345", 5), "12345");
    }

    // ── summarize_result ──────────────────────────────────────────────────────

    #[test]
    fn summarize_result_error_truncates_content() {
        let content = "a".repeat(400);
        let result = summarize_result("bash", &content, true);
        assert!(result.ends_with('…'), "should be truncated: {result}");
        assert!(
            result.chars().count() <= 302,
            "should not exceed truncation limit + ellipsis"
        );
    }

    #[test]
    fn summarize_result_glob_counts_files() {
        let content = "src/main.rs\nsrc/lib.rs\nsrc/util.rs\n";
        let result = summarize_result("glob", content, false);
        assert_eq!(result, "3 file(s) matched");
    }

    #[test]
    fn summarize_result_glob_empty_returns_no_matches() {
        assert_eq!(summarize_result("glob", "", false), "(no matches)");
        assert_eq!(summarize_result("glob", "\n\n", false), "(no matches)");
    }

    #[test]
    fn summarize_result_grep_counts_matches_and_files() {
        let content = "src/main.rs:10:fn main\nsrc/lib.rs:5:fn helper\nsrc/lib.rs:20:fn other\n";
        let result = summarize_result("grep", content, false);
        assert_eq!(result, "3 match(es) in 2 file(s)");
    }

    #[test]
    fn summarize_result_grep_empty_returns_no_matches() {
        assert_eq!(summarize_result("grep", "", false), "(no matches)");
    }

    #[test]
    fn summarize_result_read_file_counts_lines() {
        let content = "line1\nline2\nline3\n";
        let result = summarize_result("read_file", content, false);
        assert_eq!(result, "3 lines");
    }

    #[test]
    fn summarize_result_bash_single_line() {
        let result = summarize_result("bash", "hello world", false);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn summarize_result_bash_multiline_shows_first_plus_count() {
        let content = "first line\nsecond line\nthird line\n";
        let result = summarize_result("bash", content, false);
        assert!(
            result.contains("first line"),
            "should contain first line: {result}"
        );
        assert!(result.contains("+2"), "should show +2 more lines: {result}");
    }

    #[test]
    fn summarize_result_bash_empty_returns_no_output() {
        assert_eq!(summarize_result("bash", "", false), "(no output)");
        assert_eq!(summarize_result("bash", "   \n  ", false), "(no output)");
    }

    #[test]
    fn summarize_result_write_file_returns_content() {
        let result = summarize_result("write_file", "written 42 bytes", false);
        assert_eq!(result, "written 42 bytes");
    }

    #[test]
    fn summarize_result_unknown_tool_truncates_at_200() {
        let content = "x".repeat(300);
        let result = summarize_result("unknown_tool", &content, false);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 202);
    }
}
