use anyhow::{Result, bail};
use colored::Colorize;
use std::io::{self, Write};
use std::process::Command;
use std::collections::HashSet;
use std::time::Duration;

/// Permissions granted for this session (commands that bypass the prompt).
#[derive(Default)]
pub struct BashPermissions {
    always_allow: HashSet<String>,
}

impl BashPermissions {
    pub fn is_allowed(&self, cmd: &str) -> bool {
        // Check if any allowed prefix matches
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
) -> Result<String> {
    if !permissions.is_allowed(command) {
        let prefix = command.split_whitespace().next().unwrap_or(command);
        println!("\n{}", "⚡ bash command:".yellow());
        println!("  {}", command.white());
        println!("  {}  allow", "[1]".dimmed());
        println!("  {}  always allow '{}' this session", "[2]".dimmed(), prefix);
        println!("  {}  deny", "[3]".dimmed());
        print!("{} ", "[1]:".cyan());
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim() {
            "" | "1" => {}
            "2" => {
                permissions.allow(prefix);
                println!("{}", format!("'{prefix}' allowed for this session").dimmed());
            }
            _ => {
                bail!("command denied by user");
            }
        }
    }

    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command);

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    // Spawn with timeout to prevent hanging the session
    let mut child = cmd.stdout(std::process::Stdio::piped())
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

    let output = child.wait_with_output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut combined = stdout;
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    // Cap output to avoid blowing up context
    let max_chars = 10_000;
    if combined.len() > max_chars {
        let truncated = &combined[..max_chars];
        let total = combined.len();
        combined = format!("{truncated}\n\n[… truncated — {total} total chars, showing first {max_chars}]");
    }

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        if combined.is_empty() {
            bail!("command exited with code {code}");
        }
        return Ok(format!("[exit code {code}]\n{combined}"));
    }

    Ok(combined)
}
