use crate::backend::{OutputSink, sink_writeln};
use anyhow::{Result, bail};
use colored::Colorize;
use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

/// Permissions granted for this session (commands that bypass the prompt).
#[derive(Default)]
pub struct BashPermissions {
    always_allow: HashSet<String>,
    /// When true, all commands are auto-approved (TUI mode).
    /// Commands are still logged to the output sink.
    pub auto_approve: bool,
}

impl BashPermissions {
    pub fn is_allowed(&self, cmd: &str) -> bool {
        if self.auto_approve {
            return true;
        }
        self.always_allow
            .iter()
            .any(|p| cmd.starts_with(p.as_str()))
    }

    pub fn allow(&mut self, prefix: impl Into<String>) {
        self.always_allow.insert(prefix.into());
    }
}

/// Execute a bash command asynchronously.
/// Permission prompt (if needed) is sync and brief; process wait runs on the blocking thread pool.
pub async fn run_bash(
    command: &str,
    permissions: &mut BashPermissions,
    working_dir: Option<&str>,
    output: &OutputSink,
) -> Result<String> {
    if !permissions.is_allowed(command) {
        let prefix = command.split_whitespace().next().unwrap_or(command);
        sink_writeln(output, &format!("\n{}", "⚡ bash command:".yellow()));
        sink_writeln(output, &format!("  {}", command.white()));

        println!("  {}  allow", "[1]".dimmed());
        println!(
            "  {}  always allow '{}' this session",
            "[2]".dimmed(),
            prefix
        );
        println!("  {}  deny", "[3]".dimmed());
        print!("{} ", "[1]:".cyan());
        std::io::Write::flush(&mut std::io::stdout())?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        match input.trim() {
            "" | "1" => {}
            "2" => {
                permissions.allow(prefix);
                sink_writeln(
                    output,
                    &format!("'{prefix}' allowed for this session")
                        .dimmed()
                        .to_string(),
                );
            }
            _ => bail!("command denied by user"),
        }
    } else if permissions.auto_approve {
        sink_writeln(output, &format!("{} {}", "⚡".yellow(), command.dimmed()));
    }

    let command = command.to_string();
    let working_dir = working_dir.map(str::to_string);

    let output_result = tokio::task::spawn_blocking(move || -> Result<std::process::Output> {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&command);
        if let Some(dir) = &working_dir {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let timeout = Duration::from_secs(120);
        let start = std::time::Instant::now();
        loop {
            match child.try_wait()? {
                Some(_) => break,
                None => {
                    if start.elapsed() > timeout {
                        _ = child.kill();
                        bail!("command timed out after {}s", timeout.as_secs());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Ok(child.wait_with_output()?)
    })
    .await??;

    let stdout = String::from_utf8_lossy(&output_result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output_result.stderr).to_string();

    let mut combined = stdout;
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    let max_chars = 10_000;
    let char_count = combined.chars().count();
    if char_count > max_chars {
        let truncated: String = combined.chars().take(max_chars).collect();
        combined = format!(
            "{truncated}\n\n[… truncated — {char_count} total chars, showing first {max_chars}]"
        );
    }

    if !output_result.status.success() {
        let code = output_result.status.code().unwrap_or(-1);
        if combined.is_empty() {
            bail!("command exited with code {code}");
        }
        return Ok(format!("[exit code {code}]\n{combined}"));
    }

    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_sink;

    // ── BashPermissions ───────────────────────────────────────────────────────

    #[test]
    fn is_allowed_default_denies_all() {
        let perms = BashPermissions::default();
        assert!(!perms.is_allowed("ls -la"));
        assert!(!perms.is_allowed("echo hello"));
    }

    #[test]
    fn is_allowed_auto_approve_allows_all() {
        let perms = BashPermissions {
            auto_approve: true,
            ..Default::default()
        };
        assert!(perms.is_allowed("rm -rf /"));
        assert!(perms.is_allowed("any command at all"));
    }

    #[test]
    fn allow_prefix_grants_prefix_match() {
        let mut perms = BashPermissions::default();
        perms.allow("git");
        assert!(perms.is_allowed("git status"));
        assert!(perms.is_allowed("git log --oneline"));
        assert!(!perms.is_allowed("echo not git"));
    }

    #[test]
    fn allow_multiple_prefixes() {
        let mut perms = BashPermissions::default();
        perms.allow("cargo");
        perms.allow("echo");
        assert!(perms.is_allowed("cargo test"));
        assert!(perms.is_allowed("echo hello"));
        assert!(!perms.is_allowed("rm file"));
    }

    #[test]
    fn allow_exact_prefix_does_not_match_shorter() {
        let mut perms = BashPermissions::default();
        perms.allow("git status");
        assert!(perms.is_allowed("git status --short"));
        assert!(!perms.is_allowed("git")); // prefix is longer than command
    }

    // ── run_bash (auto_approve mode) ──────────────────────────────────────────

    #[tokio::test]
    async fn run_bash_runs_simple_command() {
        let (sink, _buf) = buffer_sink();
        let mut perms = BashPermissions {
            auto_approve: true,
            ..Default::default()
        };
        let result = run_bash("echo hello", &mut perms, None, &sink)
            .await
            .unwrap();
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn run_bash_captures_stderr() {
        let (sink, _buf) = buffer_sink();
        let mut perms = BashPermissions {
            auto_approve: true,
            ..Default::default()
        };
        let result = run_bash("echo err >&2", &mut perms, None, &sink)
            .await
            .unwrap();
        assert!(
            result.contains("err"),
            "stderr should be captured: {result}"
        );
    }

    #[tokio::test]
    async fn run_bash_nonzero_exit_includes_exit_code() {
        let (sink, _buf) = buffer_sink();
        let mut perms = BashPermissions {
            auto_approve: true,
            ..Default::default()
        };
        // Produce output so we get the "[exit code N]" prefix rather than an Err
        let result = run_bash("echo failing; exit 42", &mut perms, None, &sink)
            .await
            .unwrap();
        assert!(
            result.contains("42"),
            "exit code should appear in output: {result}"
        );
    }

    #[tokio::test]
    async fn run_bash_nonzero_exit_with_no_output_errors() {
        let (sink, _buf) = buffer_sink();
        let mut perms = BashPermissions {
            auto_approve: true,
            ..Default::default()
        };
        let result = run_bash("exit 1", &mut perms, None, &sink);
        // exit 1 with no output → returns Err
        assert!(result.await.is_err());
    }

    #[tokio::test]
    async fn run_bash_respects_working_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (sink, _buf) = buffer_sink();
        let mut perms = BashPermissions {
            auto_approve: true,
            ..Default::default()
        };
        let result = run_bash("pwd", &mut perms, Some(tmp.path().to_str().unwrap()), &sink)
            .await
            .unwrap();
        assert!(
            result.trim().contains(tmp.path().to_str().unwrap()),
            "pwd should reflect working_dir: {result}"
        );
    }
}
