use crate::backend::{sink_writeln, OutputSink};
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
        self.always_allow.iter().any(|p| cmd.starts_with(p.as_str()))
    }

    pub fn allow(&mut self, prefix: impl Into<String>) {
        self.always_allow.insert(prefix.into());
    }
}

/// Execute a bash command, prompting for permission if not pre-approved.
/// Returns the command's stdout+stderr combined.
pub fn run_bash(
    command: &str,
    permissions: &mut BashPermissions,
    working_dir: Option<&str>,
    output: &OutputSink,
) -> Result<String> {
    if !permissions.is_allowed(command) {
        // Interactive permission prompt (classic/readline mode only)
        let prefix = command.split_whitespace().next().unwrap_or(command);
        sink_writeln(output, &format!("\n{}", "⚡ bash command:".yellow()));
        sink_writeln(output, &format!("  {}", command.white()));

        // In auto-approve mode we already returned true above,
        // so this branch only runs in classic mode where stdin works.
        println!("  {}  allow", "[1]".dimmed());
        println!("  {}  always allow '{}' this session", "[2]".dimmed(), prefix);
        println!("  {}  deny", "[3]".dimmed());
        print!("{} ", "[1]:".cyan());
        std::io::Write::flush(&mut std::io::stdout())?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        match input.trim() {
            "" | "1" => {}
            "2" => {
                permissions.allow(prefix);
                sink_writeln(output, &format!("'{prefix}' allowed for this session").dimmed().to_string());
            }
            _ => {
                bail!("command denied by user");
            }
        }
    } else if permissions.auto_approve {
        // TUI mode: show what's running without blocking
        sink_writeln(output, &format!("{} {}", "⚡".yellow(), command.dimmed()));
    }

    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command);

    if let Some(dir) = working_dir {
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
            Some(_status) => break,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    bail!("command timed out after {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    let output_result = child.wait_with_output()?;

    let stdout = String::from_utf8_lossy(&output_result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output_result.stderr).to_string();

    let mut combined = stdout;
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    // Cap output to avoid blowing up context (char-safe truncation)
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
