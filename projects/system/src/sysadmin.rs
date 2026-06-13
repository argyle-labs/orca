//! Host-level lifecycle helpers backing `system.kill` (this file) and the
//! service-user bootstrap path used by `system.install` (in `commands.rs`).
//!
//! Service-user creation / group management / linger / SSH key install are
//! exposed as `pub(crate)` helpers so the install tool can drive them.
//! There is no dedicated `system.bootstrap` orca_tool — install owns that
//! responsibility now.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use colored::Colorize;
use contract::ToolCtx;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SystemKillArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct SystemKillOutput {
    pub killed_patterns: Vec<String>,
}

/// Kill stale orca runtime processes (mcp-serve, daemon start) so a binary
/// swap is picked up by their clients on next call. Safe to run before any
/// deploy; no-op when nothing matches.
#[orca_tool(domain = "system", verb = "kill")]
async fn system_kill(_args: SystemKillArgs, _ctx: &ToolCtx) -> Result<SystemKillOutput> {
    let mut killed = Vec::new();
    for pat in STALE_PATTERNS {
        let status = Command::new("pkill").arg("-f").arg(pat).status();
        match status {
            Ok(s) if s.success() => {
                println!("→ killed processes matching '{pat}'");
                killed.push((*pat).to_string());
            }
            Ok(_) => {}
            Err(e) => eprintln!("warn: pkill '{pat}' failed: {e}"),
        }
    }
    Ok(SystemKillOutput {
        killed_patterns: killed,
    })
}

const STALE_PATTERNS: &[&str] = &["orca mcp-serve", "orca daemon"];

/// Default home directory when `system.install --service-user <u>` is
/// called without an explicit `--home-dir`. The user name itself is
/// already required at the call site, so no default constant is needed.
pub(crate) const DEFAULT_SERVICE_HOME: &str = "/var/lib/orca";

/// Create the orca service user and configure SSH access. Idempotent.
/// Designed to run as root immediately after the binary is placed, before
/// `daemon install --service-user orca`. Driven by `system.install` —
/// there is no standalone `system.bootstrap` orca_tool.
#[cfg(target_os = "linux")]
pub(crate) fn bootstrap(admin_pubkey: Option<String>, user: &str, home_dir: &str) -> Result<()> {
    validate_shell_safe("--service-user", user)?;
    validate_shell_safe("--home-dir", home_dir)?;

    let user_exists = Command::new("id")
        .arg(user)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if user_exists {
        println!("{} user '{user}' already exists", "-".dimmed());
    } else {
        create_service_user(user, home_dir)?;
        add_to_groups(user);
    }

    enable_linger(user);

    if let Some(pk) = admin_pubkey {
        install_ssh_key(user, home_dir, &pk)?;
    }

    println!("{} bootstrap: user={user}, home={home_dir}", "✓".green());
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn bootstrap(admin_pubkey: Option<String>, _user: &str, home_dir: &str) -> Result<()> {
    if let Some(pk) = admin_pubkey {
        install_ssh_key("", home_dir, &pk)?;
    }
    println!(
        "{} service user management not applicable on this OS",
        "-".dimmed()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_service_user(user: &str, home_dir: &str) -> Result<()> {
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };

    let ok = if utils::path::which("useradd").is_some() {
        Command::new("useradd")
            .args([
                "--system",
                "--create-home",
                "--home-dir",
                home_dir,
                "--shell",
                shell,
                user,
            ])
            .status()?
            .success()
    } else if utils::path::which("adduser").is_some() {
        Command::new("adduser")
            .args(["-S", "-D", "-h", home_dir, "-s", shell, user])
            .status()?
            .success()
    } else {
        anyhow::bail!("neither useradd nor adduser found — cannot create user '{user}'");
    };

    if !ok {
        anyhow::bail!("failed to create service user '{user}'");
    }
    println!(
        "{} created service user '{user}' (home: {home_dir})",
        "✓".green()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn add_to_groups(user: &str) {
    for grp in &["docker", "systemd-journal"] {
        let exists = Command::new("getent")
            .args(["group", grp])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !exists {
            continue;
        }

        let ok = if utils::path::which("usermod").is_some() {
            Command::new("usermod")
                .args(["-aG", grp, user])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else if utils::path::which("addgroup").is_some() {
            Command::new("addgroup")
                .args([user, grp])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            false
        };

        if ok {
            println!("{} added '{user}' to group '{grp}'", "✓".green());
        } else {
            eprintln!("warn: could not add '{user}' to group '{grp}'");
        }
    }
}

#[cfg(target_os = "linux")]
fn enable_linger(user: &str) {
    if !std::path::Path::new("/run/systemd/system").exists() {
        return;
    }
    let ok = Command::new("loginctl")
        .args(["enable-linger", user])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("{} enabled linger for '{user}'", "✓".green());
    } else {
        eprintln!("warn: loginctl enable-linger failed (non-fatal on non-systemd hosts)");
    }
}

fn install_ssh_key(user: &str, home_dir: &str, pubkey: &str) -> Result<()> {
    let ssh_dir = std::path::PathBuf::from(home_dir).join(".ssh");
    std::fs::create_dir_all(&ssh_dir)?;
    let auth = ssh_dir.join("authorized_keys");
    std::fs::write(&auth, format!("{pubkey}\n"))?;

    #[cfg(unix)]
    {
        std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))?;
    }

    #[cfg(target_os = "linux")]
    if !user.is_empty() && is_root() {
        let chown = Command::new("chown")
            .args(["-R", user])
            .arg(&ssh_dir)
            .status()
            .with_context(|| format!("invoking chown -R {user} on {}", ssh_dir.display()))?;
        if !chown.success() {
            anyhow::bail!(
                "chown -R {user} {} failed with status {chown} — SSH key would be unreadable to {user}",
                ssh_dir.display()
            );
        }
    }

    println!("{} installed SSH key for '{user}'", "✓".green());
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn validate_shell_safe(label: &str, s: &str) -> Result<()> {
    if s.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | '@'))
    {
        Ok(())
    } else {
        anyhow::bail!(
            "{label} '{s}' contains characters not safe to interpolate into a shell script \
             (allowed: alphanumeric, _, -, /, ., @)"
        )
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[test]
    fn validate_shell_safe_accepts_valid() {
        validate_shell_safe("user", "orca").unwrap();
        validate_shell_safe("user", "my-service_user").unwrap();
        validate_shell_safe("home", "/var/lib/orca").unwrap();
    }

    #[test]
    fn validate_shell_safe_rejects_metacharacters() {
        for bad in ["orca; rm -rf /", "orca$(id)", "orca user", "orca\nnewline"] {
            assert!(
                validate_shell_safe("test", bad).is_err(),
                "expected err for: {bad}"
            );
        }
    }

    #[test]
    fn validate_shell_safe_rejects_empty() {
        assert!(validate_shell_safe("f", "").is_err());
    }
}
