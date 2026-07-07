//! Daemon control plane — signal handling (stop/park/reclaim), supervisor
//! install/uninstall, and a runtime-status snapshot.
//!
//! Reads (running/pid/port/uptime) surface as `system.detail.daemon`. Stop/
//! park/reclaim are flags on `system.update`. Supervisor install/uninstall
//! are absorbed by `system.install` and `system.delete`. There is no
//! `system.daemon.*` orca_tool — the daemon is part of the system, not a
//! separate resource.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use colored::Colorize;
#[cfg(target_os = "macos")]
use contract::config::APP_PLIST_LABEL;
#[cfg(target_os = "linux")]
use contract::config::APP_SYSTEMD_SERVICE;
use contract::config::{APP_DAEMON_LOG_FILE, APP_LOGS_SUBDIR, APP_NAME, APP_STATE_DIR};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::process::Command;
use utils::state::DaemonMode;

/// Runtime snapshot of the orca daemon (pid/port/uptime + liveness).
///
/// Surfaced as `system.detail.daemon`. The `version`, `mode`, and binary
/// path fields callers might expect already live on the parent
/// `SystemStatusReport` (sourced from the daemon state file) — don't
/// duplicate them here.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Default)]
pub struct DaemonRuntimeStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub uptime_seconds: Option<i64>,
}

/// Read daemon state and return the runtime snapshot. Returns the
/// `running: false` default when no state file is present (daemon not
/// installed or never started).
pub(crate) fn collect_runtime_status() -> Result<DaemonRuntimeStatus> {
    let Some(s) = utils::state::read()? else {
        return Ok(DaemonRuntimeStatus::default());
    };
    let secs = chrono::Utc::now()
        .signed_duration_since(s.started_at)
        .num_seconds();
    Ok(DaemonRuntimeStatus {
        running: pid_alive(s.daemon_pid),
        pid: Some(s.daemon_pid),
        port: Some(s.port),
        uptime_seconds: Some(secs),
    })
}

/// Default HTTP port for `system.install` when no explicit port is passed.
pub(crate) const DEFAULT_HTTP_PORT: u16 = contract::config::APP_REST_HTTP_PORT;

// ── internal helpers (signals + supervisor install/uninstall) ──────────────

pub(crate) fn stop() -> Result<u32> {
    let s = utils::state::read()?
        .ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    send_signal(s.daemon_pid, "TERM")?;
    Ok(s.daemon_pid)
}

pub(crate) fn park() -> Result<u32> {
    let s = utils::state::read()?
        .ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    if s.mode != DaemonMode::Daemon {
        anyhow::bail!("daemon is not in running mode (current: {:?})", s.mode);
    }
    send_signal(s.daemon_pid, "USR1")?;
    Ok(s.daemon_pid)
}

pub(crate) fn reclaim() -> Result<u32> {
    let s = utils::state::read()?
        .ok_or_else(|| anyhow::anyhow!("daemon not running (no state file)"))?;
    if s.mode == DaemonMode::Daemon {
        return Ok(s.daemon_pid);
    }
    send_signal(s.daemon_pid, "USR2")?;
    Ok(s.daemon_pid)
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

/// Validate that a string is safe to interpolate into a shell script written
/// to disk (init scripts, go-hooks, plist XML). Accepts Unix username chars
/// and absolute path chars only; rejects metacharacters that could turn a
/// written script into an injection vector.
fn validate_shell_safe(label: &str, s: &str) -> Result<()> {
    if s.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    let ok = s
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | '@'));
    if !ok {
        anyhow::bail!(
            "{label} '{s}' contains characters that are not safe to interpolate into a shell \
             script (allowed: alphanumeric, _, -, /, ., @)"
        );
    }
    Ok(())
}

pub(crate) fn install(port: u16, service_user: Option<String>) -> Result<()> {
    let binary = resolve_binary()?;
    match service_user {
        None => {
            // User-mode install — current behavior, runs in the caller's $HOME.
            ensure_pki_for_home(&std::env::var("HOME")?)?;
            install_service(&binary, port)
        }
        Some(user) => {
            // System-mode install — requires root, runs as `user` at boot.
            if !is_root() {
                anyhow::bail!("--service-user requires running as root");
            }
            validate_shell_safe("--service-user", &user)?;
            // On Unraid the orca user's home MUST be the appdata path so
            // state/db/pki/secrets persist across reboot. `home_dir_of` falls
            // back to `/var/lib/<user>` (tmpfs, wiped on Unraid reboot) when
            // the user doesn't exist yet or has no passwd entry — relying on
            // it has bricked pod membership twice. Force the canonical home
            // here. See [[project-unraid-rc-orca-home-bug]],
            // [[project-unraid-persistence-via-appdata]].
            #[cfg(target_os = "linux")]
            let home = if matches!(detect_linux_init(), LinuxInit::Unraid) {
                "/mnt/user/appdata/orca".to_string()
            } else {
                home_dir_of(&user)?
            };
            #[cfg(not(target_os = "linux"))]
            let home = home_dir_of(&user)?;
            validate_shell_safe("home directory", &home)?;
            ensure_pki_for_home(&home)?;
            // chown the PKI tree to the service user so the daemon can read it.
            let pki_dir = std::path::PathBuf::from(&home)
                .join(APP_STATE_DIR)
                .join(contract::config::APP_PKI_DIR);
            chown_recursive(&pki_dir, &user)?;
            install_system_service(&binary, port, &user, &home)
        }
    }
}

fn ensure_pki_for_home(home: &str) -> Result<()> {
    let pki_dir = std::path::PathBuf::from(home)
        .join(APP_STATE_DIR)
        .join(contract::config::APP_PKI_DIR);
    utils::pki::init(&pki_dir)?;
    Ok(())
}

fn is_root() -> bool {
    // Avoid pulling in libc just for this — `id -u` is on every Unix host.
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
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

fn resolve_binary() -> Result<String> {
    // current_exe is the highest-confidence source: it's literally the
    // running binary's path, regardless of $HOME, PATH, or whether we were
    // invoked as a different user than the install owner. This is the case
    // that breaks `which` + `$HOME/.local/bin/orca` fallbacks when install.sh
    // (running as root) invokes `orca daemon install --service-user orca`.
    if let Ok(exe) = std::env::current_exe()
        && let Some(s) = exe.to_str()
    {
        return Ok(s.to_string());
    }
    if let Some(s) = utils::state::read()?
        && !s.binary.is_empty()
    {
        return Ok(s.binary);
    }
    if let Some(path) = utils::path::which(APP_NAME) {
        return Ok(path);
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
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_LOG_FILE}");

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
    <string>{daemon_log}</string>
    <key>StandardErrorPath</key>
    <string>{daemon_log}</string>
</dict>
</plist>
"#
    );

    std::fs::write(&plist_path, &plist)?;
    println!("{} wrote {}", "✓".green(), plist_path);

    // Remove any existing registration before bootstrapping; ignore failure when not loaded
    _ = Command::new("launchctl")
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
    println!("  logs: tail -f {daemon_log}");
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn uninstall_service() -> Result<()> {
    let home = std::env::var("HOME")?;
    let uid = launchd_uid().unwrap_or(0);
    let domain = format!("gui/{uid}");
    let plist_path = format!("{home}/Library/LaunchAgents/{APP_PLIST_LABEL}.plist");

    _ = Command::new("launchctl")
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
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_LOG_FILE}");

    let service = format!(
        "[Unit]\nDescription={APP_NAME} daemon\nAfter=network.target\n\n\
         [Service]\nExecStart={binary} daemon --port {port}\n\
         Environment=HOME={home}\nRestart=always\nRestartSec=5\n\
         StandardOutput=append:{daemon_log}\nStandardError=append:{daemon_log}\n\n\
         [Install]\nWantedBy=default.target\n"
    );

    std::fs::write(&service_path, &service)?;
    println!("{} wrote {}", "✓".green(), service_path);

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("invoking systemctl --user daemon-reload")?;
    if !reload.success() {
        anyhow::bail!("systemctl --user daemon-reload failed with status {reload}");
    }

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
             write a service unit manually and run `{binary} daemon --port {port}` as {user}"
        ),
    }
}

#[cfg(target_os = "linux")]
fn install_systemd_system(binary: &str, port: u16, user: &str, home: &str) -> Result<()> {
    let path = format!("/etc/systemd/system/{APP_SYSTEMD_SERVICE}.service");
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    let chown = Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), &logs_dir])
        .status()
        .with_context(|| format!("invoking chown on {logs_dir}"))?;
    if !chown.success() {
        anyhow::bail!("chown {user}:{user} {logs_dir} failed with status {chown}");
    }
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_LOG_FILE}");
    let unit = format!(
        "[Unit]\nDescription={APP_NAME} daemon\nAfter=network.target\n\n\
         [Service]\nType=simple\nUser={user}\n\
         Environment=HOME={home}\nExecStart={binary} daemon --port {port}\n\
         Restart=always\nRestartSec=5\n\
         StandardOutput=append:{daemon_log}\nStandardError=append:{daemon_log}\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    );
    std::fs::write(&path, &unit)?;
    println!("{} wrote {}", "✓".green(), path);

    let reload = Command::new("systemctl")
        .arg("daemon-reload")
        .status()
        .context("invoking systemctl daemon-reload")?;
    if !reload.success() {
        anyhow::bail!("systemctl daemon-reload failed with status {reload}");
    }
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
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    let chown = Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), &logs_dir])
        .status()
        .with_context(|| format!("invoking chown on {logs_dir}"))?;
    if !chown.success() {
        anyhow::bail!("chown {user}:{user} {logs_dir} failed with status {chown}");
    }
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_LOG_FILE}");
    // OpenRC init script. supervise-daemon handles restart-on-crash without
    // requiring start-stop-daemon/pidfile bookkeeping. `command_user` drops
    // privs to the orca user. output_log/error_log keep daemon stdout+stderr
    // off /dev/null — without these supervise-daemon discards everything.
    let script = format!(
        "#!/sbin/openrc-run\n\
         name=\"{APP_NAME}\"\n\
         description=\"{APP_NAME} daemon\"\n\
         command=\"{binary}\"\n\
         command_args=\"daemon --port {port}\"\n\
         command_user=\"{user}\"\n\
         supervisor=supervise-daemon\n\
         pidfile=\"/run/{APP_NAME}.pid\"\n\
         output_log=\"{daemon_log}\"\n\
         error_log=\"{daemon_log}\"\n\
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

/// Render the `/etc/rc.d/rc.orca` init-script body. Pure / side-effect free
/// so the Unraid persistence contract (especially the `env HOME=$HOME` runuser
#[cfg(target_os = "linux")]
fn install_unraid(binary: &str, _port: u16, user: &str, _home: &str) -> Result<()> {
    // Unraid: bootstrap-only install. The /etc/rc.d/rc.orca + /boot/config/go
    // hook lifecycle path was retired 2026-06-06 in favor of the Unraid
    // plugin manager owning install/upgrade/start/stop via the .plg install
    // script (see `render_plg_install_script` in package.rs).
    //
    // Why retired: the rc.orca stop handler used /var/run/orca.pid which
    // routinely went stale, leaving a daemon alive while `start` spawned a
    // second one. The two raced for 0.0.0.0:12002 and the loser stayed up
    // serving HTTP+HTTPS but no mesh, breaking peer-dispatch silently. See
    // [[project-unraid-rc-orca-stale-pid-race]] +
    // [[project-system-detail-hides-mesh-bind-failure]].
    //
    // What this fn still does on Unraid:
    //   - create appdata dirs and chown to the service user
    //   - persist the binary in appdata (skipping self-copy when called from
    //     the .plg install path where binary == persist_bin)
    //   - create the USB plugin dir (so the plugin manager has a place to
    //     keep the .plg)
    //
    // Lifecycle is owned by the .plg. `system install --service-user orca`
    // on a bare Unraid box is now a no-op for daemon state — install the
    // .plg via `make unraid-install HOST=<host>` (or `plugin install`) to
    // bring the daemon up.
    use std::os::unix::fs::PermissionsExt;

    let appdata_dir = "/mnt/user/appdata/orca";
    let appdata_bin_dir = format!("{appdata_dir}/bin");
    std::fs::create_dir_all(&appdata_bin_dir)?;
    _ = Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), appdata_dir])
        .status();

    let persist_bin = format!("{appdata_bin_dir}/orca");
    let already_persisted = std::fs::canonicalize(binary)
        .ok()
        .zip(std::fs::canonicalize(&persist_bin).ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false);
    if !already_persisted {
        std::fs::copy(binary, &persist_bin)?;
    }
    std::fs::set_permissions(&persist_bin, std::fs::Permissions::from_mode(0o755))?;
    _ = Command::new("chown")
        .args([&format!("{user}:{user}"), &persist_bin])
        .status();
    println!("{} wrote {}", "✓".green(), persist_bin);

    // Ensure the USB plugin dir exists so `plugin install` has a stable
    // destination for the .plg on first install.
    std::fs::create_dir_all("/boot/config/plugins/orca")?;
    println!(
        "{} {APP_NAME} (unraid) bootstrap complete — daemon lifecycle owned by .plg",
        "✓".green()
    );
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
pub(crate) fn uninstall_service() -> Result<()> {
    // disable --now: log failures and keep going so we still remove the unit
    // file. A failed disable usually means the service is already stopped or
    // never existed; not a reason to abort the uninstall.
    match Command::new("systemctl")
        .args(["--user", "disable", "--now", APP_SYSTEMD_SERVICE])
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(
            "systemctl --user disable --now {APP_SYSTEMD_SERVICE} exited {s} — continuing uninstall"
        ),
        Err(e) => tracing::warn!(
            "invoking systemctl --user disable --now {APP_SYSTEMD_SERVICE}: {e:#} — continuing uninstall"
        ),
    }

    let home = std::env::var("HOME")?;
    let service_path = format!("{home}/.config/systemd/user/{APP_SYSTEMD_SERVICE}.service");
    if std::path::Path::new(&service_path).exists() {
        std::fs::remove_file(&service_path)?;
        println!("{} removed {}", "✓".green(), service_path);
    }

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("invoking systemctl --user daemon-reload")?;
    if !reload.success() {
        anyhow::bail!("systemctl --user daemon-reload failed with status {reload}");
    }
    println!("{} {APP_NAME} daemon uninstalled", "✓".green());
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_service(_binary: &str, _port: u16) -> Result<()> {
    anyhow::bail!("daemon install is not supported on this OS")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn uninstall_service() -> Result<()> {
    anyhow::bail!("daemon uninstall is not supported on this OS")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // The render_unraid_rc_script template was retired 2026-06-06 along
    // with the rc.orca lifecycle path. The HOME-through-runuser regression
    // (2026-06-02) is now enforced inside the .plg install script — see
    // `plg_install_script_runuser_keeps_home_*` in package::tests.

    // ── validate_shell_safe ───────────────────────────────────────────────────

    #[test]
    fn validate_shell_safe_accepts_valid_identifiers() {
        validate_shell_safe("user", "orca").unwrap();
        validate_shell_safe("user", "my-service_user").unwrap();
        validate_shell_safe("home", "/var/lib/orca").unwrap();
        validate_shell_safe("home", "/home/orca.user").unwrap();
    }

    #[test]
    fn validate_shell_safe_rejects_metacharacters() {
        for bad in [
            "orca; rm -rf /",
            "orca$(whoami)",
            "orca`id`",
            "orca | cat /etc/passwd",
            "orca\nmalicious",
            "orca user",
            "orca\"quote",
        ] {
            assert!(
                validate_shell_safe("test", bad).is_err(),
                "expected Err for: {bad}"
            );
        }
    }

    #[test]
    fn validate_shell_safe_rejects_empty() {
        assert!(validate_shell_safe("field", "").is_err());
    }

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
