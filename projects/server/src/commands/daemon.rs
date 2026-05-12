use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
#[cfg(target_os = "linux")]
use orca_utils::config::APP_SYSTEMD_SERVICE;
#[cfg(target_os = "macos")]
use orca_utils::config::{APP_DAEMON_LOG, APP_PLIST_LABEL};
use orca_utils::config::{APP_NAME, APP_STATE_DIR};
use orca_utils::state::DaemonMode;
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
    /// Install and enable as a system service (launchd on macOS, systemd/openrc/unraid on Linux)
    Install {
        #[arg(short, long, default_value = "12000")]
        port: u16,
        /// Install as a SYSTEM service running as this user (requires root).
        /// Required on OpenRC and Unraid; optional on systemd (otherwise installs
        /// a per-user service into the current user's systemd --user manager).
        #[arg(long)]
        service_user: Option<String>,
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
        DaemonAction::Install { port, service_user } => install(port, service_user),
        DaemonAction::Uninstall => uninstall(),
    }
}

fn status() -> Result<()> {
    let Some(s) = orca_utils::state::read()? else {
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
        println!(
            "  {}",
            "warning: PID not found — daemon may have crashed".yellow()
        );
        println!(
            "  {}",
            format!("hint: remove ~/{APP_STATE_DIR}/state.json and restart").dimmed()
        );
    }
    Ok(())
}

fn stop() -> Result<()> {
    let s = orca_utils::state::read()?
        .ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    send_signal(s.daemon_pid, "TERM")?;
    println!(
        "{} sent SIGTERM to daemon (pid {})",
        "✓".green(),
        s.daemon_pid
    );
    Ok(())
}

fn park() -> Result<()> {
    let s = orca_utils::state::read()?
        .ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    if s.mode != DaemonMode::Daemon {
        anyhow::bail!("daemon is not in running mode (current: {:?})", s.mode);
    }
    send_signal(s.daemon_pid, "USR1")?;
    println!(
        "{} parked daemon (pid {}) — port {} released",
        "✓".green(),
        s.daemon_pid,
        s.port
    );
    Ok(())
}

fn reclaim() -> Result<()> {
    let s = orca_utils::state::read()?
        .ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    if s.mode == DaemonMode::Daemon {
        println!(
            "{} daemon is already running on port {}",
            "✓".green(),
            s.port
        );
        return Ok(());
    }
    send_signal(s.daemon_pid, "USR2")?;
    println!(
        "{} sent SIGUSR2 to daemon (pid {}) — reclaiming port {}",
        "✓".green(),
        s.daemon_pid,
        s.port
    );
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

fn install(port: u16, service_user: Option<String>) -> Result<()> {
    let binary = resolve_binary()?;
    match service_user {
        None => {
            // User-mode install — current behavior, runs in the caller's $HOME.
            ensure_pki_for_home(&std::env::var("HOME")?)?;
            install_service(&binary, port)
        }
        Some(user) => {
            // System-mode install — requires root, runs as `user` at boot.
            #[cfg(unix)]
            if !is_root() {
                anyhow::bail!("--service-user requires running as root");
            }
            let home = home_dir_of(&user)?;
            ensure_pki_for_home(&home)?;
            // chown the PKI tree to the service user so the daemon can read it.
            let pki_dir = std::path::PathBuf::from(&home)
                .join(APP_STATE_DIR)
                .join(orca_utils::config::APP_PKI_DIR);
            chown_recursive(&pki_dir, &user)?;
            install_system_service(&binary, port, &user, &home)
        }
    }
}

fn ensure_pki_for_home(home: &str) -> Result<()> {
    let pki_dir = std::path::PathBuf::from(home)
        .join(APP_STATE_DIR)
        .join(orca_utils::config::APP_PKI_DIR);
    orca_sdk::pki::init(&pki_dir)?;
    Ok(())
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: getuid is always safe; it has no preconditions and cannot fail.
    unsafe { libc::getuid() == 0 }
}

fn home_dir_of(user: &str) -> Result<String> {
    let out = Command::new("getent").args(["passwd", user]).output()?;
    if !out.status.success() {
        anyhow::bail!("getent passwd {user} failed — user does not exist?");
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim()
        .split(':')
        .nth(5)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("could not parse home dir for {user} from getent"))
}

fn chown_recursive(path: &std::path::Path, user: &str) -> Result<()> {
    let status = Command::new("chown")
        .args(["-R", user])
        .arg(path)
        .status()?;
    if !status.success() {
        anyhow::bail!("chown -R {user} {} failed", path.display());
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    uninstall_service()
}

fn resolve_binary() -> Result<String> {
    if let Some(s) = orca_utils::state::read()?
        && !s.binary.is_empty()
    {
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
    println!(
        "{} {APP_NAME} daemon installed — starts now and on login",
        "✓".green()
    );
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

// ── System-mode install (root) — picks systemd / openrc / unraid by init ──

#[cfg(target_os = "linux")]
fn detect_linux_init() -> LinuxInit {
    use std::path::Path;
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    if os_release.contains("ID=\"unraid-os\"") || os_release.contains("ID=unraid-os") {
        return LinuxInit::Unraid;
    }
    if Path::new("/run/systemd/system").exists() {
        return LinuxInit::Systemd;
    }
    if Path::new("/run/openrc").exists() || Path::new("/sbin/openrc").exists() {
        return LinuxInit::Openrc;
    }
    LinuxInit::Unknown
}

#[cfg(target_os = "linux")]
enum LinuxInit {
    Systemd,
    Openrc,
    Unraid,
    Unknown,
}

#[cfg(target_os = "linux")]
fn install_system_service(binary: &str, port: u16, user: &str, home: &str) -> Result<()> {
    match detect_linux_init() {
        LinuxInit::Systemd => install_systemd_system(binary, port, user, home),
        LinuxInit::Openrc => install_openrc(binary, port, user, home),
        LinuxInit::Unraid => install_unraid(binary, port, user, home),
        LinuxInit::Unknown => anyhow::bail!(
            "could not detect init system (not systemd, openrc, or unraid) — \
             write a service unit manually and run `{binary} daemon start --port {port}` as {user}"
        ),
    }
}

#[cfg(target_os = "linux")]
fn install_systemd_system(binary: &str, port: u16, user: &str, home: &str) -> Result<()> {
    let path = format!("/etc/systemd/system/{APP_SYSTEMD_SERVICE}.service");
    let unit = format!(
        "[Unit]\nDescription=Orca AI daemon\nAfter=network.target\n\n\
         [Service]\nType=simple\nUser={user}\n\
         Environment=HOME={home}\nExecStart={binary} daemon start --port {port}\n\
         Restart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    );
    std::fs::write(&path, &unit)?;
    println!("{} wrote {}", "✓".green(), path);

    let _ = Command::new("systemctl").arg("daemon-reload").status();
    let status = Command::new("systemctl")
        .args(["enable", "--now", APP_SYSTEMD_SERVICE])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl enable --now {APP_SYSTEMD_SERVICE} failed");
    }
    println!(
        "{} {APP_NAME} system daemon enabled and started",
        "✓".green()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_openrc(binary: &str, port: u16, user: &str, home: &str) -> Result<()> {
    let path = format!("/etc/init.d/{APP_SYSTEMD_SERVICE}");
    // OpenRC init script. supervise-daemon handles restart-on-crash without
    // requiring start-stop-daemon/pidfile bookkeeping. `command_user` drops
    // privs to the orca user; `command_background=true` would conflict with
    // supervise-daemon, so we omit it.
    let script = format!(
        "#!/sbin/openrc-run\n\
         name=\"{APP_NAME}\"\n\
         description=\"Orca AI daemon\"\n\
         command=\"{binary}\"\n\
         command_args=\"daemon start --port {port}\"\n\
         command_user=\"{user}\"\n\
         supervisor=supervise-daemon\n\
         pidfile=\"/run/{APP_NAME}.pid\"\n\
         export HOME=\"{home}\"\n\
         depend() {{\n    need net\n}}\n"
    );
    std::fs::write(&path, &script)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    println!("{} wrote {}", "✓".green(), path);

    let status = Command::new("rc-update")
        .args(["add", APP_SYSTEMD_SERVICE, "default"])
        .status()?;
    if !status.success() {
        anyhow::bail!("rc-update add {APP_SYSTEMD_SERVICE} default failed");
    }
    let status = Command::new("rc-service")
        .args([APP_SYSTEMD_SERVICE, "start"])
        .status()?;
    if !status.success() {
        anyhow::bail!("rc-service {APP_SYSTEMD_SERVICE} start failed");
    }
    println!("{} {APP_NAME} (openrc) enabled and started", "✓".green());
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_unraid(binary: &str, port: u16, user: &str, home: &str) -> Result<()> {
    // Unraid wipes most of `/` on reboot — only `/boot` is persistent. So we
    // install both:
    //   1. /etc/rc.d/rc.orca         — runtime init script for the current boot
    //   2. /boot/config/plugins/orca/orca.go  — persistent startup hook
    // and append a call from /boot/config/go so it survives reboots.
    let rc_path = format!("/etc/rc.d/rc.{APP_NAME}");
    let rc_script = format!(
        "#!/bin/sh\n\
         # Orca daemon (Unraid). Generated by `orca daemon install`.\n\
         BIN={binary}\n\
         USER={user}\n\
         HOME={home}\n\
         export HOME\n\
         case \"$1\" in\n\
           start) runuser -u $USER -- $BIN daemon start --port {port} >>/var/log/orca.log 2>&1 &\n\
                  echo $! > /var/run/orca.pid ;;\n\
           stop)  [ -f /var/run/orca.pid ] && kill $(cat /var/run/orca.pid) ; rm -f /var/run/orca.pid ;;\n\
           restart) $0 stop; sleep 1; $0 start ;;\n\
           *) echo \"usage: $0 {{start|stop|restart}}\"; exit 1 ;;\n\
         esac\n"
    );
    std::fs::write(&rc_path, &rc_script)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&rc_path, std::fs::Permissions::from_mode(0o755))?;
    println!("{} wrote {}", "✓".green(), rc_path);

    // Persistent copy on the boot USB.
    let persist_dir = "/boot/config/plugins/orca";
    std::fs::create_dir_all(persist_dir)?;
    let persist_path = format!("{persist_dir}/rc.{APP_NAME}");
    std::fs::copy(&rc_path, &persist_path)?;
    println!("{} wrote {}", "✓".green(), persist_path);

    // Hook into /boot/config/go so the script is re-installed + started on every boot.
    let go_path = "/boot/config/go";
    let marker = "# --- orca daemon (managed by `orca daemon install`) ---";
    let hook = format!(
        "\n{marker}\n\
         cp -f {persist_path} {rc_path}\n\
         chmod +x {rc_path}\n\
         {rc_path} start\n\
         # --- end orca daemon ---\n"
    );
    let mut existing = std::fs::read_to_string(go_path).unwrap_or_default();
    if !existing.contains(marker) {
        existing.push_str(&hook);
        std::fs::write(go_path, &existing)?;
        println!("{} appended startup hook to {}", "✓".green(), go_path);
    } else {
        println!(
            "{} {} already has orca hook (skipped)",
            "✓".green(),
            go_path
        );
    }

    // Start now.
    let status = Command::new(&rc_path).arg("start").status()?;
    if !status.success() {
        anyhow::bail!("{rc_path} start failed");
    }
    println!("{} {APP_NAME} (unraid) installed and started", "✓".green());
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_system_service(_binary: &str, _port: u16, _user: &str, _home: &str) -> Result<()> {
    anyhow::bail!(
        "--service-user is not yet supported on macOS (use the per-user LaunchAgent path)"
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_system_service(_binary: &str, _port: u16, _user: &str, _home: &str) -> Result<()> {
    anyhow::bail!("--service-user is not supported on this OS")
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
        assert!(
            result.is_ok(),
            "resolve_binary should never error: {:?}",
            result
        );
        let path = result.unwrap();
        assert!(!path.is_empty(), "resolve_binary returned an empty string");
    }
}
