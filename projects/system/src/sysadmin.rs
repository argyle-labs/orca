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
use contract::config::APP_NAME;
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

/// Result of a selective `mcp-serve` reap: which pids were signalled and how
/// many same-binary instances were intentionally left running.
pub(crate) struct ReapOutcome {
    /// Pids that were sent SIGTERM (stale — started before the deploy boundary).
    pub killed: Vec<u32>,
    /// Count of matching `mcp-serve` processes left alone because they started
    /// at/after the boundary (i.e. already on the freshly-installed binary, or
    /// a client that reconnected mid-deploy).
    pub spared: usize,
}

/// True when `(cmd, name)` identifies an `orca mcp-serve` stdio server.
///
/// Matches on the binary identity (process name is `orca`, or argv[0]'s
/// basename is `orca`) plus an `mcp-serve` argument — mirrors the
/// `"orca mcp-serve"` entry in [`STALE_PATTERNS`] but as a structured
/// predicate so the deploy reap can be selective rather than a blanket
/// `pkill`. Pure and platform-independent so it is unit-testable.
fn is_mcp_serve(cmd: &[String], name: &str) -> bool {
    let looks_like_orca = name == APP_NAME
        || cmd
            .first()
            .and_then(|arg0| arg0.rsplit('/').next())
            .is_some_and(|base| base == APP_NAME);
    looks_like_orca && cmd.iter().any(|arg| arg == "mcp-serve")
}

/// Terminate `orca mcp-serve` processes that started before `boundary_unix`
/// (epoch seconds) — i.e. instances running a binary image from before the
/// just-completed deploy. Their MCP clients (Claude Code) respawn a fresh
/// server on the new binary at the next reconnect.
///
/// Same-binary instances (start time at/after the boundary) and this process
/// itself are left untouched, so a deploy never severs a session that already
/// reconnected, and the daemon — which carries `daemon`, not `mcp-serve` — is
/// out of scope here (it is restarted separately by the supervisor reinstall).
#[cfg(unix)]
pub(crate) fn reap_stale_mcp_serve(boundary_unix: u64) -> ReapOutcome {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    let self_pid = std::process::id();
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let mut killed = Vec::new();
    let mut spared = 0;
    for proc in sys.processes().values() {
        let pid = proc.pid().as_u32();
        if pid == self_pid {
            continue;
        }
        let name = proc.name().to_string_lossy();
        let cmd: Vec<String> = proc
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        if !is_mcp_serve(&cmd, &name) {
            continue;
        }
        if proc.start_time() >= boundary_unix {
            spared += 1;
            continue;
        }
        if send_sigterm(pid) {
            killed.push(pid);
        }
    }
    ReapOutcome { killed, spared }
}

#[cfg(not(unix))]
pub(crate) fn reap_stale_mcp_serve(_boundary_unix: u64) -> ReapOutcome {
    ReapOutcome {
        killed: Vec::new(),
        spared: 0,
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> bool {
    Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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

    // A matching primary group MUST exist for the later `chown <user>:<user>`
    // in `daemon::install`. `useradd` (Debian/Arch) creates it implicitly, but
    // BusyBox `adduser` (Alpine) does not — and on a re-install where the user
    // already exists we skip `create_service_user` entirely, so the group can
    // be permanently absent. Ensure it unconditionally (idempotent).
    ensure_service_group(user);

    enable_linger(user);

    // Grant the daemon the one privileged capability it needs: applying autofs
    // config (write /etc/auto.* + restart autofs) via the scoped admin helper.
    // Without this the storage self-heal / failover surface is inert (the daemon
    // runs unprivileged and can't touch root-owned /etc). Best-effort: a failure
    // here shouldn't abort the whole install.
    if let Err(e) = install_autofs_sudoers(user, home_dir) {
        eprintln!("{} autofs sudoers rule not installed: {e}", "!".yellow());
    }

    if let Some(pk) = admin_pubkey {
        install_ssh_key(user, home_dir, &pk)?;
    }

    println!("{} bootstrap: user={user}, home={home_dir}", "✓".green());
    Ok(())
}

/// Install `/etc/sudoers.d/orca`: a single NOPASSWD grant letting the service
/// user run exactly `<home>/.local/bin/orca admin storage-apply` as root — the
/// one privileged seam for autofs config. Scoped to that command with no
/// wildcard (the payload rides on stdin), validated with `visudo -cf` before it
/// takes effect, and removed again if validation fails so a broken drop-in can
/// never wedge sudo.
#[cfg(target_os = "linux")]
fn install_autofs_sudoers(user: &str, home_dir: &str) -> Result<()> {
    validate_shell_safe("--service-user", user)?;
    validate_shell_safe("--home-dir", home_dir)?;
    if !is_root() {
        anyhow::bail!("must be root to write /etc/sudoers.d");
    }

    let binary = format!("{}/.local/bin/orca", home_dir.trim_end_matches('/'));
    let path = "/etc/sudoers.d/orca";
    let contents = format!(
        "# Managed by orca — do not edit.\n\
         # Lets the unprivileged orca daemon apply autofs config (write\n\
         # /etc/auto.* + restart autofs) via the scoped admin helper. The\n\
         # payload is passed on stdin, so no argument wildcard is needed.\n\
         {user} ALL=(root) NOPASSWD: {binary} admin storage-apply\n"
    );

    std::fs::write(path, &contents).with_context(|| format!("write {path}"))?;
    // sudoers drop-ins must be 0440 or sudo ignores them.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o440))
        .with_context(|| format!("chmod {path}"))?;

    // Validate; a bad drop-in would break sudo host-wide, so remove it on failure.
    let ok = Command::new("visudo")
        .args(["-cf", path])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true); // no visudo → assume the syntax (which we control) is fine
    if !ok {
        let _ = std::fs::remove_file(path);
        anyhow::bail!("visudo rejected {path} (removed)");
    }

    println!(
        "{} sudoers: {user} may run 'orca admin storage-apply'",
        "✓".green()
    );
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
    // `www-data` is the Proxmox pmxcfs read group — /etc/pve/{lxc,qemu-server}/*.conf
    // are mode 640 root:www-data. Without it the proxmox topology collector
    // silently returns no claims and the systems tree never nests VMs under
    // their host. Best-effort like the others — non-Proxmox hosts won't have
    // the group, which is fine.
    for grp in &["docker", "systemd-journal", "www-data"] {
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

/// Ensure a group named `user` exists and the service user belongs to it, so
/// `chown <user>:<user>` succeeds during daemon install. Idempotent,
/// best-effort — every step tolerates already-present state.
#[cfg(target_os = "linux")]
fn ensure_service_group(user: &str) {
    let group_exists = Command::new("getent")
        .args(["group", user])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !group_exists {
        let made = if utils::path::which("groupadd").is_some() {
            Command::new("groupadd")
                .args(["-r", user])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else if utils::path::which("addgroup").is_some() {
            // BusyBox addgroup: `-S` creates a system group.
            Command::new("addgroup")
                .args(["-S", user])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            false
        };
        if made {
            println!("{} created service group '{user}'", "✓".green());
        } else {
            eprintln!("warn: could not create service group '{user}'");
        }
    }

    // Make the user a member (harmless if already a member / primary group).
    let ok = if utils::path::which("usermod").is_some() {
        Command::new("usermod")
            .args(["-aG", user, user])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else if utils::path::which("addgroup").is_some() {
        // BusyBox 2-arg form: `addgroup USER GROUP`.
        Command::new("addgroup")
            .args([user, user])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        false
    };
    if !ok {
        eprintln!("warn: could not add '{user}' to group '{user}' (non-fatal if already a member)");
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

#[cfg(test)]
mod reap_tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matches_bare_orca_mcp_serve() {
        assert!(is_mcp_serve(&args(&["orca", "mcp-serve"]), "orca"));
    }

    #[test]
    fn matches_absolute_path_argv0() {
        assert!(is_mcp_serve(
            &args(&["/Users/dev/.local/bin/orca", "mcp-serve"]),
            "orca",
        ));
    }

    #[test]
    fn ignores_the_daemon() {
        assert!(!is_mcp_serve(
            &args(&["orca", "daemon", "--port", "12000"]),
            "orca"
        ));
    }

    #[test]
    fn ignores_other_orca_subcommands() {
        assert!(!is_mcp_serve(&args(&["orca", "system", "install"]), "orca"));
    }

    #[test]
    fn ignores_unrelated_process_that_merely_has_an_mcp_serve_arg() {
        // A non-orca binary carrying an `mcp-serve` argument must not match.
        assert!(!is_mcp_serve(
            &args(&["/usr/bin/python3", "mcp-serve"]),
            "python3"
        ));
    }

    #[test]
    fn matches_when_name_is_orca_even_if_argv0_is_a_wrapper() {
        // sysinfo reports the executable name as `orca`; argv[0] may be a
        // login-shell wrapper. The name check carries the match.
        assert!(is_mcp_serve(&args(&["-orca", "mcp-serve"]), "orca"));
    }
}
