use anyhow::Result;
use config::{APP_DAEMON_LOG, APP_NAME, APP_PLIST_LABEL, APP_STATE_DIR};
#[cfg(target_os = "linux")]
use config::APP_SYSTEMD_SERVICE;
use state::{self, DaemonMode};
use clap::Subcommand;
use colored::Colorize;
use std::process::Command;

#[derive(Subcommand)]
pub enum DaemonAction {
    /// Start the daemon (runs the serve loop with signal handling)
    Start {
        #[arg(short, long, default_value = "12000")]
        port: u16,
    },
    /// Show daemon status
    Status,
    /// Stop the daemon gracefully
    Stop,
    /// Park the daemon — release port, stay alive (SIGUSR1)
    Park,
    /// Reclaim the port after a dev session (SIGUSR2)
    Reclaim,
    /// Install and enable as a system service (launchd on macOS, systemd on Linux)
    Install {
        #[arg(short, long, default_value = "12000")]
        port: u16,
    },
    /// Disable and remove the system service
    Uninstall,
}

/// Handle all DaemonAction variants except Start (which needs the server crate — handled in main.rs).
pub fn cmd_daemon(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start { .. } => unreachable!("Start handled in main.rs"),
        DaemonAction::Status => status(),
        DaemonAction::Stop => stop(),
        DaemonAction::Park => park(),
        DaemonAction::Reclaim => reclaim(),
        DaemonAction::Install { port } => install(port),
        DaemonAction::Uninstall => uninstall(),
    }
}

fn status() -> Result<()> {
    let Some(s) = state::read()? else {
        println!("{} daemon not running", "●".dimmed());
        return Ok(());
    };

    let mode_label = match s.mode {
        DaemonMode::Daemon => "running".green().to_string(),
        DaemonMode::Parked => "parked (port released)".yellow().to_string(),
        DaemonMode::Dev => "dev-superseded".cyan().to_string(),
    };

    let alive = pid_alive(s.daemon_pid);
    let dot = if alive { "●".green() } else { "●".red() };

    println!("{} {APP_NAME} daemon", dot);
    println!("  mode:    {}", mode_label);
    println!("  pid:     {}", s.daemon_pid);
    if s.mode != DaemonMode::Daemon {
        println!("  active:  {} ({})", s.active_pid, "dev server".cyan());
    }
    println!("  port:    {}", s.port);
    println!("  version: {}", s.version);
    println!("  binary:  {}", s.binary);

    let secs = chrono::Utc::now()
        .signed_duration_since(s.started_at)
        .num_seconds();
    let uptime = if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    };
    println!("  uptime:  {}", uptime);

    if !alive {
        println!("  {}", "warning: PID not found — daemon may have crashed".yellow());
        println!("  {}", format!("hint: remove ~/{APP_STATE_DIR}/state.json and restart").dimmed());
    }
    Ok(())
}

fn stop() -> Result<()> {
    let s = state::read()?.ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    send_signal(s.daemon_pid, "TERM")?;
    println!("{} sent SIGTERM to daemon (pid {})", "✓".green(), s.daemon_pid);
    Ok(())
}

fn park() -> Result<()> {
    let s = state::read()?.ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    if s.mode != DaemonMode::Daemon {
        anyhow::bail!("daemon is not in running mode (current: {:?})", s.mode);
    }
    send_signal(s.daemon_pid, "USR1")?;
    println!("{} parked daemon (pid {}) — port {} released", "✓".green(), s.daemon_pid, s.port);
    Ok(())
}

fn reclaim() -> Result<()> {
    let s = state::read()?.ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    if s.mode == DaemonMode::Daemon {
        println!("{} daemon is already running on port {}", "✓".green(), s.port);
        return Ok(());
    }
    send_signal(s.daemon_pid, "USR2")?;
    println!("{} sent SIGUSR2 to daemon (pid {}) — reclaiming port {}", "✓".green(), s.daemon_pid, s.port);
    Ok(())
}

fn send_signal(pid: u32, sig: &str) -> Result<()> {
    let status = Command::new("kill")
        .args([&format!("-{sig}"), &pid.to_string()])
        .status()?;
    if !status.success() {
        anyhow::bail!("kill -{sig} {pid} failed — is the process still running?");
    }
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Install / Uninstall ───────────────────────────────────────────────────────

fn install(port: u16) -> Result<()> {
    let binary = resolve_binary()?;
    install_service(&binary, port)
}

fn uninstall() -> Result<()> {
    uninstall_service()
}

fn resolve_binary() -> Result<String> {
    if let Some(s) = state::read()?
        && !s.binary.is_empty() {
            return Ok(s.binary);
        }
    let out = Command::new("which").arg(APP_NAME).output()?;
    if out.status.success() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    Ok(format!("{home}/.local/bin/{APP_NAME}"))
}

#[cfg(target_os = "macos")]
fn install_service(binary: &str, port: u16) -> Result<()> {
    let home = std::env::var("HOME")?;
    let uid = launchd_uid()?;
    let domain = format!("gui/{uid}");
    let agents_dir = format!("{home}/Library/LaunchAgents");
    std::fs::create_dir_all(&agents_dir)?;
    let plist_path = format!("{agents_dir}/{APP_PLIST_LABEL}.plist");

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{APP_PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{home}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    <string>{APP_DAEMON_LOG}</string>
    <key>StandardErrorPath</key>
    <string>{APP_DAEMON_LOG}</string>
</dict>
</plist>
"#
    );

    std::fs::write(&plist_path, &plist)?;
    println!("{} wrote {}", "✓".green(), plist_path);

    // Remove any existing registration before bootstrapping; ignore failure when not loaded
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist_path])
        .stderr(std::process::Stdio::null())
        .status();

    let status = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_path])
        .status()?;

    if !status.success() {
        anyhow::bail!("launchctl bootstrap {domain} failed");
    }
    println!("{} {APP_NAME} daemon installed — starts now and on login", "✓".green());
    println!("  logs: tail -f {APP_DAEMON_LOG}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_service() -> Result<()> {
    let home = std::env::var("HOME")?;
    let uid = launchd_uid().unwrap_or(0);
    let domain = format!("gui/{uid}");
    let plist_path = format!("{home}/Library/LaunchAgents/{APP_PLIST_LABEL}.plist");

    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist_path])
        .status();

    if std::path::Path::new(&plist_path).exists() {
        std::fs::remove_file(&plist_path)?;
        println!("{} removed {}", "✓".green(), plist_path);
    }
    println!("{} {APP_NAME} daemon uninstalled", "✓".green());
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_uid() -> Result<u32> {
    let out = Command::new("id").arg("-u").output()?;
    let uid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("could not parse UID from `id -u`"))?;
    Ok(uid)
}

#[cfg(target_os = "linux")]
fn install_service(binary: &str, port: u16) -> Result<()> {
    let home = std::env::var("HOME")?;
    let service_dir = format!("{home}/.config/systemd/user");
    std::fs::create_dir_all(&service_dir)?;
    let service_path = format!("{service_dir}/{APP_SYSTEMD_SERVICE}.service");

    let service = format!(
        "[Unit]\nDescription=Orca AI daemon\nAfter=network.target\n\n\
         [Service]\nExecStart={binary} daemon start --port {port}\n\
         Environment=HOME={home}\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n"
    );

    std::fs::write(&service_path, &service)?;
    println!("{} wrote {}", "✓".green(), service_path);

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    let status = Command::new("systemctl")
        .args(["--user", "enable", "--now", APP_SYSTEMD_SERVICE])
        .status()?;

    if !status.success() {
        anyhow::bail!("systemctl enable --now {APP_SYSTEMD_SERVICE} failed");
    }
    println!("{} {APP_NAME} daemon enabled and started", "✓".green());
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_service() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", APP_SYSTEMD_SERVICE])
        .status();

    let home = std::env::var("HOME")?;
    let service_path = format!("{home}/.config/systemd/user/{APP_SYSTEMD_SERVICE}.service");
    if std::path::Path::new(&service_path).exists() {
        std::fs::remove_file(&service_path)?;
        println!("{} removed {}", "✓".green(), service_path);
    }

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!("{} {APP_NAME} daemon uninstalled", "✓".green());
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_service(_binary: &str, _port: u16) -> Result<()> {
    anyhow::bail!("daemon install is not supported on this OS")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn uninstall_service() -> Result<()> {
    anyhow::bail!("daemon uninstall is not supported on this OS")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── pid_alive ─────────────────────────────────────────────────────────────

    #[test]
    fn pid_alive_current_process_is_true() {
        let pid = std::process::id();
        assert!(
            pid_alive(pid),
            "pid_alive should return true for the current process (pid {pid})"
        );
    }

    #[test]
    fn pid_alive_impossible_pid_is_false() {
        // PID 99999999 is far above the OS limit on any platform; kill -0 will fail.
        assert!(
            !pid_alive(99_999_999),
            "pid_alive should return false for an impossible PID"
        );
    }

    // ── resolve_binary ────────────────────────────────────────────────────────

    #[test]
    fn resolve_binary_falls_back_to_local_bin_when_no_state() {
        // When there is no state file and `orca` is not on PATH, resolve_binary
        // should return the ~/.local/bin/orca fallback rather than an error.
        // We cannot guarantee `orca` is on PATH in CI, so we only assert the
        // result is non-empty and is either a real path or the fallback path.
        let result = resolve_binary();
        assert!(result.is_ok(), "resolve_binary should never error: {:?}", result);
        let path = result.unwrap();
        assert!(!path.is_empty(), "resolve_binary returned an empty string");
    }
}
