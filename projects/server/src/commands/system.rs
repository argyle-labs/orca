//! `orca system <verb>` — host-level lifecycle helpers that scripts shell out to.
//!
//! Each verb is a Rust function so Makefile, install.sh, deploy-host.sh, and
//! the orca binary itself agree on patterns and behavior. Adding a new
//! pattern (e.g. another process name to clean up on binary swap) is a single
//! edit here, not a sweep across shell files.

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use std::process::Command;

#[derive(Subcommand, Debug)]
pub enum SystemAction {
    /// Kill stale orca runtime processes (mcp-serve, daemon start) so a
    /// binary swap is picked up by their clients on next call. Safe to run
    /// before any deploy; no-op when nothing matches.
    KillStale,

    /// Create the orca service user and configure SSH access. Idempotent.
    /// Designed to run as root immediately after the binary is placed,
    /// before `daemon install --service-user orca`.
    Bootstrap {
        /// SSH pubkey to add to the service user's authorized_keys.
        #[arg(long)]
        admin_pubkey: Option<String>,
        /// Service user name (default: orca).
        #[arg(long, default_value = "orca")]
        service_user: String,
        /// Home directory for the service user (default: /var/lib/orca).
        #[arg(long, default_value = "/var/lib/orca")]
        home_dir: String,
    },
}

pub fn cmd_system(action: SystemAction) -> Result<()> {
    match action {
        SystemAction::KillStale => kill_stale_runtime(),
        SystemAction::Bootstrap {
            admin_pubkey,
            service_user,
            home_dir,
        } => bootstrap(admin_pubkey, &service_user, &home_dir),
    }
}

/// Patterns kept here as the single source — scripts must NOT inline pkill.
const STALE_PATTERNS: &[&str] = &["orca mcp-serve", "orca daemon start"];

pub fn kill_stale_runtime() -> Result<()> {
    for pat in STALE_PATTERNS {
        // pkill -f matches against the full argv string. Exit 1 = no match,
        // which is fine — we ignore non-zero. Other exits (2 = syntax, 3 =
        // fatal, 64+ = signal failure) we surface as warnings, not fatals,
        // because deploy must proceed.
        let status = Command::new("pkill").arg("-f").arg(pat).status();
        match status {
            Ok(s) if s.success() => println!("→ killed processes matching '{pat}'"),
            Ok(_) => {} // no match — silent
            Err(e) => eprintln!("warn: pkill '{pat}' failed: {e}"),
        }
    }
    Ok(())
}

// ── bootstrap ─────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn bootstrap(admin_pubkey: Option<String>, user: &str, home_dir: &str) -> Result<()> {
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
fn bootstrap(admin_pubkey: Option<String>, _user: &str, home_dir: &str) -> Result<()> {
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

    let ok = if tool_present("useradd") {
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
    } else if tool_present("adduser") {
        // busybox adduser (Alpine/Unraid)
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

        let ok = if tool_present("usermod") {
            Command::new("usermod")
                .args(["-aG", grp, user])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else if tool_present("addgroup") {
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
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))?;
    }

    #[cfg(target_os = "linux")]
    if !user.is_empty() && is_root() {
        let _ = Command::new("chown")
            .args(["-R", user])
            .arg(&ssh_dir)
            .status();
    }

    println!("{} installed SSH key for '{user}'", "✓".green());
    Ok(())
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn tool_present(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Validate that a string is safe to interpolate into shell scripts written
/// to disk. Accepts Unix username and path chars only.
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
