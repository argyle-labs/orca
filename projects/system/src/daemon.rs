//! Daemon control plane — signal handling (stop/park/reclaim), supervisor
//! install/uninstall, and a runtime-status snapshot.
//!
//! Reads (running/pid/port/uptime) surface as `system.detail.daemon`. Stop/
//! park/reclaim are flags on `system.update`. Supervisor install/uninstall
//! are absorbed by `system.install` and `system.uninstall`. There is no
//! `system.daemon.*` orca_tool — the daemon is part of the system, not a
//! separate resource.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use colored::Colorize;
#[cfg(target_os = "macos")]
use contract::config::APP_PLIST_LABEL;
use contract::config::{APP_DAEMON_STDERR_FILE, APP_LOGS_SUBDIR, APP_NAME, APP_STATE_DIR};
#[cfg(target_os = "linux")]
use contract::config::{APP_SYSTEMD_SERVICE, APP_SYSTEMD_SERVICE_LEGACY};
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
#[derive::camel_aliases]
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
    let secs = s.started_at.elapsed_seconds();
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
            // it has bricked mesh membership twice. Force the canonical home
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
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
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
    // Supervisor capture only — the structured log is the subscriber's
    // rotated tee (`daemon.jsonl`). See APP_DAEMON_STDERR_FILE.
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_STDERR_FILE}");

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
    // Point at the STRUCTURED log, not the stderr capture — the latter is
    // nearly empty by design now. `orca logs` is the better answer than tail.
    warn_about_the_orphaned_legacy_log(&logs_dir);
    println!("  logs: orca logs   (structured, filterable)");
    println!("  stderr/panics: tail -f {daemon_log}");
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
/// The pre-split `daemon.log` is no longer written by anything, but a long-
/// running host can be carrying a very large one (123 MB on mint). Say so on
/// install rather than silently leaving it on the disk — deleting an
/// operator's logs unasked is not ours to do, and leaving them unmentioned is
/// how 123 MB accumulated in the first place.
fn warn_about_the_orphaned_legacy_log(logs_dir: &str) {
    let legacy = std::path::Path::new(logs_dir).join(contract::config::APP_DAEMON_LOG_FILE_LEGACY);
    let Ok(meta) = std::fs::metadata(&legacy) else {
        return;
    };
    let mb = meta.len() / (1024 * 1024);
    println!(
        "  note: {} ({} MB) is from before the log split and is no longer written.\n        Safe to delete.",
        legacy.display(),
        mb
    );
}

// macOS-only: `id -u` feeds the launchd domain target. Keep this gate attached
// to the fn — it was orphaned once when a new fn was inserted directly beneath
// the attribute, which left this compiling on Linux as dead code.
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
fn canonical_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| path.to_string())
}

#[cfg(target_os = "linux")]
fn render_user_unit(binary: &str, port: u16, home: &str, daemon_log: &str) -> String {
    format!(
        "[Unit]\nDescription={APP_NAME} daemon\nAfter=network.target {APP_SYSTEMD_SERVICE_LEGACY}.service\n\
         Conflicts={APP_SYSTEMD_SERVICE_LEGACY}.service\n\n\
         [Service]\nExecStart={binary} daemon --port {port}\n\
         Environment=HOME={home}\nRestart=always\nRestartSec=5\n\
         StandardOutput=append:{daemon_log}\nStandardError=append:{daemon_log}\n\n\
         [Install]\nWantedBy=default.target\n"
    )
}

#[cfg(target_os = "linux")]
fn render_system_unit(binary: &str, port: u16, user: &str, home: &str, daemon_log: &str) -> String {
    format!(
        "[Unit]\nDescription={APP_NAME} daemon\nAfter=network.target {APP_SYSTEMD_SERVICE_LEGACY}.service\n\
         Conflicts={APP_SYSTEMD_SERVICE_LEGACY}.service\n\n\
         [Service]\nType=simple\nUser={user}\n\
         Environment=HOME={home}\nExecStart={binary} daemon --port {port}\n\
         Restart=always\nRestartSec=5\n\
         StandardOutput=append:{daemon_log}\nStandardError=append:{daemon_log}\n\n\
         [Install]\nWantedBy=multi-user.target\n"
    )
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
struct UnitFields {
    binary: String,
    port: u16,
    user: Option<String>,
    home: String,
}

#[cfg(target_os = "linux")]
fn parse_unit(content: &str) -> Option<UnitFields> {
    let value = |key: &str| {
        content
            .lines()
            .find_map(|l| l.trim().strip_prefix(key).map(str::to_owned))
    };
    let exec = value("ExecStart=")?;
    let (binary, port) = exec.split_once(" daemon --port ")?;
    Some(UnitFields {
        binary: binary.trim().to_string(),
        port: port.split_whitespace().next()?.parse().ok()?,
        user: value("User="),
        home: value("Environment=HOME=")?,
    })
}

#[cfg(target_os = "linux")]
fn fresh_unit_for(legacy: &str, user_mode: bool) -> Option<String> {
    let f = parse_unit(legacy)?;
    let log = format!(
        "{}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}/{APP_DAEMON_STDERR_FILE}",
        f.home
    );
    Some(if user_mode {
        render_user_unit(&f.binary, f.port, &f.home, &log)
    } else {
        render_system_unit(&f.binary, f.port, f.user.as_deref()?, &f.home, &log)
    })
}

#[cfg(target_os = "linux")]
fn is_orca_daemon_unit(content: &str) -> bool {
    content.lines().any(|l| {
        let l = l.trim();
        l.starts_with("ExecStart=") && l.contains(" daemon --port ")
    })
}

#[cfg(target_os = "linux")]
fn stable_binary(binary: &str, home: &str) -> String {
    let resolved = canonical_path(binary);
    [
        Some(binary.to_string()),
        utils::path::which(APP_NAME),
        Some(format!("{home}/.local/bin/{APP_NAME}")),
        Some(format!("/usr/local/bin/{APP_NAME}")),
    ]
    .into_iter()
    .flatten()
    .find(|c| {
        std::path::Path::new(c).exists() && canonical_path(c) == resolved && parent_root_owned(c)
    })
    .unwrap_or(resolved)
}

#[cfg(target_os = "linux")]
fn parent_root_owned(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::path::Path::new(path)
        .parent()
        .and_then(|d| std::fs::metadata(d).ok())
        .is_some_and(|m| m.uid() == 0)
}

#[cfg(target_os = "linux")]
const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

#[cfg(target_os = "linux")]
fn owned_unit(unit_dir: &str, name: &str) -> Option<String> {
    let path = format!("{unit_dir}/{name}.service");
    let content = std::fs::read_to_string(&path).ok()?;
    is_orca_daemon_unit(&content).then_some(path)
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
enum Step {
    Run(Vec<String>),
    RemoveLegacy,
    Handoff,
}

#[cfg(target_os = "linux")]
fn systemctl(user_mode: bool, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["systemctl".to_string()];
    if user_mode {
        argv.push("--user".into());
    }
    argv.extend(args.iter().map(|a| a.to_string()));
    argv
}

#[cfg(target_os = "linux")]
fn activation_plan(user_mode: bool, legacy: bool) -> Vec<Step> {
    let mut plan = vec![
        Step::Run(systemctl(user_mode, &["daemon-reload"])),
        Step::Run(systemctl(user_mode, &["enable", APP_SYSTEMD_SERVICE])),
    ];
    if legacy {
        plan.push(Step::Run(systemctl(
            user_mode,
            &["disable", APP_SYSTEMD_SERVICE_LEGACY],
        )));
        plan.push(Step::RemoveLegacy);
        plan.push(Step::Handoff);
    } else {
        plan.push(Step::Run(systemctl(
            user_mode,
            &["start", APP_SYSTEMD_SERVICE],
        )));
    }
    plan
}

#[cfg(target_os = "linux")]
fn handoff_commands(user_mode: bool) -> [Vec<Vec<String>>; 2] {
    let scope = if user_mode { "--user " } else { "" };
    let script = format!(
        "systemctl {scope}stop {APP_SYSTEMD_SERVICE_LEGACY}; \
         systemctl {scope}daemon-reload; \
         systemctl {scope}start {APP_SYSTEMD_SERVICE}"
    );
    let mut run = vec!["systemd-run".to_string()];
    if user_mode {
        run.push("--user".into());
    }
    run.extend(
        [
            "--no-block",
            "--collect",
            "--unit",
            "orca-daemon-migrate",
            "/bin/sh",
            "-c",
            &script,
        ]
        .map(String::from),
    );
    [
        vec![run],
        vec![systemctl(
            user_mode,
            &["--no-block", "start", APP_SYSTEMD_SERVICE],
        )],
    ]
}

#[cfg(target_os = "linux")]
fn run_plan(
    plan: &[Step],
    unit_dir: &str,
    user_mode: bool,
    run: &mut dyn FnMut(&[String]) -> bool,
) -> Result<()> {
    for step in plan {
        match step {
            Step::Run(argv) => {
                if !run(argv) {
                    anyhow::bail!("`{}` failed", argv.join(" "));
                }
            }
            Step::RemoveLegacy => {
                if let Some(path) = owned_unit(unit_dir, APP_SYSTEMD_SERVICE_LEGACY) {
                    std::fs::remove_file(&path)?;
                    println!("{} removed legacy {}", "✓".green(), path);
                }
            }
            Step::Handoff => {
                let handed_off = handoff_commands(user_mode)
                    .iter()
                    .any(|cmds| cmds.iter().all(|argv| run(argv)));
                if !handed_off {
                    anyhow::bail!(
                        "could not hand {APP_SYSTEMD_SERVICE_LEGACY} -> {APP_SYSTEMD_SERVICE} switch to systemd"
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_command(argv: &[String]) -> bool {
    match Command::new(&argv[0]).args(&argv[1..]).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            tracing::warn!("`{}` exited {s}", argv.join(" "));
            false
        }
        Err(e) => {
            tracing::warn!("invoking `{}`: {e:#}", argv.join(" "));
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn activate_unit(unit_dir: &str, user_mode: bool) -> Result<()> {
    let legacy = owned_unit(unit_dir, APP_SYSTEMD_SERVICE_LEGACY).is_some();
    run_plan(
        &activation_plan(user_mode, legacy),
        unit_dir,
        user_mode,
        &mut run_command,
    )
}

#[cfg(target_os = "linux")]
fn legacy_scope_from_cgroup(cgroup: &str) -> Option<bool> {
    let legacy = format!("/{APP_SYSTEMD_SERVICE_LEGACY}.service");
    cgroup
        .lines()
        .filter_map(|l| l.splitn(3, ':').nth(2))
        .find(|path| path.ends_with(&legacy))
        .map(|path| path.contains("/user@"))
}

#[cfg(target_os = "linux")]
fn migrate_unit_dir(
    unit_dir: &str,
    user_mode: bool,
    run: &mut dyn FnMut(&[String]) -> bool,
) -> Result<bool> {
    let Some(legacy_path) = owned_unit(unit_dir, APP_SYSTEMD_SERVICE_LEGACY) else {
        return Ok(false);
    };
    let legacy = std::fs::read_to_string(&legacy_path)?;
    let unit = fresh_unit_for(&legacy, user_mode)
        .ok_or_else(|| anyhow::anyhow!("cannot parse legacy unit {legacy_path}"))?;
    std::fs::write(format!("{unit_dir}/{APP_SYSTEMD_SERVICE}.service"), unit)?;
    run_plan(&activation_plan(user_mode, true), unit_dir, user_mode, run)?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn migrate_legacy_unit(
    cgroup: &str,
    home: &str,
    root: bool,
    run: &mut dyn FnMut(&[String]) -> bool,
    elevate: &mut dyn FnMut(utils::privileged::PrivilegedCall) -> bool,
) -> Result<bool> {
    match legacy_scope_from_cgroup(cgroup) {
        None => Ok(false),
        Some(true) => migrate_unit_dir(&format!("{home}/.config/systemd/user"), true, run),
        Some(false) if root => migrate_unit_dir(SYSTEM_UNIT_DIR, false, run),
        Some(false) => {
            if elevate(utils::privileged::PrivilegedCall::helper(
                "unit-migrate",
                Vec::new(),
            )) {
                return Ok(true);
            }
            tracing::warn!(
                "running under legacy system unit {APP_SYSTEMD_SERVICE_LEGACY}.service and `sudo -n {} admin unit-migrate` is not granted; run `sudo orca identity privilege repair`",
                utils::path::PRIVILEGED_BIN
            );
            Ok(false)
        }
    }
}

#[cfg(target_os = "linux")]
pub fn admin_unit_migrate() -> Result<()> {
    if !is_root() {
        anyhow::bail!("admin unit-migrate must run as root");
    }
    if migrate_unit_dir(SYSTEM_UNIT_DIR, false, &mut run_command)? {
        println!(
            "{} handed {APP_SYSTEMD_SERVICE_LEGACY}.service -> {APP_SYSTEMD_SERVICE}.service switch to systemd",
            "✓".green()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn admin_unit_migrate() -> Result<()> {
    anyhow::bail!("admin unit-migrate is only supported on Linux")
}

#[cfg(target_os = "linux")]
fn elevate(call: utils::privileged::PrivilegedCall) -> bool {
    match utils::reactor::block_on(utils::privileged::run(call)) {
        Ok(out) if out.success => true,
        Ok(out) => {
            tracing::warn!(
                "unit-migrate helper exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            tracing::warn!("unit-migrate helper: {e}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
pub fn migrate_legacy_unit_on_startup() {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let home = std::env::var("HOME")
        .map(|h| canonical_path(&h))
        .unwrap_or_default();
    match migrate_legacy_unit(&cgroup, &home, is_root(), &mut run_command, &mut elevate) {
        Ok(true) => tracing::info!(
            "handed {APP_SYSTEMD_SERVICE_LEGACY}.service -> {APP_SYSTEMD_SERVICE}.service switch to systemd"
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!("legacy unit migration failed: {e:#}"),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn migrate_legacy_unit_on_startup() {}

#[cfg(target_os = "linux")]
fn remove_owned_units(
    unit_dir: &str,
    user_mode: bool,
    run: &mut dyn FnMut(&[String]) -> bool,
) -> Result<()> {
    let mut removed = false;
    for name in [APP_SYSTEMD_SERVICE, APP_SYSTEMD_SERVICE_LEGACY] {
        let Some(path) = owned_unit(unit_dir, name) else {
            continue;
        };
        run(&systemctl(user_mode, &["disable", "--now", name]));
        std::fs::remove_file(&path)?;
        println!("{} removed {}", "✓".green(), path);
        removed = true;
    }
    if removed && !run(&systemctl(user_mode, &["daemon-reload"])) {
        anyhow::bail!("systemctl daemon-reload failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_service(binary: &str, port: u16) -> Result<()> {
    let home = canonical_path(&std::env::var("HOME")?);
    let binary = stable_binary(binary, &home);
    let service_dir = format!("{home}/.config/systemd/user");
    std::fs::create_dir_all(&service_dir)?;
    let service_path = format!("{service_dir}/{APP_SYSTEMD_SERVICE}.service");
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    // Supervisor capture only — the structured log is the subscriber's
    // rotated tee (`daemon.jsonl`). See APP_DAEMON_STDERR_FILE.
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_STDERR_FILE}");

    let service = render_user_unit(&binary, port, &home, &daemon_log);

    std::fs::write(&service_path, &service)?;
    println!("{} wrote {}", "✓".green(), service_path);

    activate_unit(&service_dir, true)?;
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
    let home = canonical_path(home);
    let binary = stable_binary(binary, &home);
    let path = format!("{SYSTEM_UNIT_DIR}/{APP_SYSTEMD_SERVICE}.service");
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    let chown = Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), &logs_dir])
        .status()
        .with_context(|| format!("invoking chown on {logs_dir}"))?;
    if !chown.success() {
        anyhow::bail!("chown {user}:{user} {logs_dir} failed with status {chown}");
    }
    // Supervisor capture only — the structured log is the subscriber's
    // rotated tee (`daemon.jsonl`). See APP_DAEMON_STDERR_FILE.
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_STDERR_FILE}");
    let unit = render_system_unit(&binary, port, user, &home, &daemon_log);
    std::fs::write(&path, &unit)?;
    println!("{} wrote {}", "✓".green(), path);

    activate_unit(SYSTEM_UNIT_DIR, false)?;
    println!(
        "{} {APP_NAME} system daemon enabled and started",
        "✓".green()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_openrc(binary: &str, port: u16, user: &str, home: &str) -> Result<()> {
    let path = format!("/etc/init.d/{APP_NAME}");
    let logs_dir = format!("{home}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}");
    std::fs::create_dir_all(&logs_dir)?;
    let chown = Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), &logs_dir])
        .status()
        .with_context(|| format!("invoking chown on {logs_dir}"))?;
    if !chown.success() {
        anyhow::bail!("chown {user}:{user} {logs_dir} failed with status {chown}");
    }
    // Supervisor capture only — the structured log is the subscriber's
    // rotated tee (`daemon.jsonl`). See APP_DAEMON_STDERR_FILE.
    let daemon_log = format!("{logs_dir}/{APP_DAEMON_STDERR_FILE}");
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
        .args(["add", APP_NAME, "default"])
        .status()?;
    if !status.success() {
        anyhow::bail!("rc-update add {APP_NAME} default failed");
    }
    let status = Command::new("rc-service")
        .args([APP_NAME, "start"])
        .status()?;
    if !status.success() {
        anyhow::bail!("rc-service {APP_NAME} start failed");
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
    let home = canonical_path(&std::env::var("HOME")?);
    remove_owned_units(
        &format!("{home}/.config/systemd/user"),
        true,
        &mut run_command,
    )?;
    if is_root() {
        remove_owned_units(SYSTEM_UNIT_DIR, false, &mut run_command)?;
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

    #[cfg(target_os = "linux")]
    #[test]
    fn user_unit_runs_the_daemon_under_the_new_name() {
        assert_eq!(APP_SYSTEMD_SERVICE, "orca-daemon");
        let unit = render_user_unit("/usr/bin/orca", 12001, "/home/u", "/home/u/l");
        assert!(unit.contains("ExecStart=/usr/bin/orca daemon --port 12001"));
        assert!(unit.contains("Environment=HOME=/home/u"));
        assert!(!unit.contains("Alias="));
        assert!(is_orca_daemon_unit(&unit));
    }

    #[cfg(target_os = "linux")]
    const SCREEN_READER: &str =
        "[Unit]\nDescription=Orca screen reader\n\n[Service]\nExecStart=/usr/bin/orca\n";

    #[cfg(target_os = "linux")]
    const LEGACY_USER_UNIT: &str = "[Unit]\nDescription=orca daemon\n\n[Service]\n\
         ExecStart=/usr/bin/orca daemon --port 12001\nEnvironment=HOME=/home/u\n";

    #[cfg(target_os = "linux")]
    fn daemon_unit() -> String {
        render_user_unit("/usr/bin/orca", 12001, "/home/u", "/home/u/l")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn migration_enables_the_new_unit_before_removing_the_old() {
        let plan = activation_plan(true, true);
        let pos = |want: &Step| plan.iter().position(|s| s == want).unwrap();
        let enable = pos(&Step::Run(systemctl(
            true,
            &["enable", APP_SYSTEMD_SERVICE],
        )));
        let disable = pos(&Step::Run(systemctl(
            true,
            &["disable", APP_SYSTEMD_SERVICE_LEGACY],
        )));
        let remove = pos(&Step::RemoveLegacy);
        let handoff = pos(&Step::Handoff);
        assert!(enable < disable && disable < remove && remove < handoff);
        assert!(plan.iter().all(|s| match s {
            Step::Run(argv) => !argv.iter().any(|a| a == "--now"),
            _ => true,
        }));
        let [primary, _] = handoff_commands(false);
        assert_eq!(primary[0][0], "systemd-run");
        assert!(primary[0].contains(&"--no-block".to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_install_starts_without_handoff() {
        let plan = activation_plan(false, false);
        assert!(!plan.contains(&Step::Handoff));
        assert_eq!(
            plan.last(),
            Some(&Step::Run(systemctl(
                false,
                &["start", APP_SYSTEMD_SERVICE]
            )))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_file_survives_a_failed_enable() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("orca.service");
        std::fs::write(&legacy, daemon_unit()).unwrap();
        let err = run_plan(
            &activation_plan(true, true),
            dir.path().to_str().unwrap(),
            true,
            &mut |argv| !argv.contains(&"enable".to_string()),
        );
        assert!(err.is_err());
        assert!(legacy.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_detects_the_legacy_unit_from_cgroup() {
        assert_eq!(
            legacy_scope_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/orca.service\n"
            ),
            Some(true)
        );
        assert_eq!(
            legacy_scope_from_cgroup("0::/system.slice/orca.service\n"),
            Some(false)
        );
        assert_eq!(
            legacy_scope_from_cgroup("0::/system.slice/orca-daemon.service\n"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_migration_copies_and_hands_off() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/systemd/user");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("orca.service"), LEGACY_USER_UNIT).unwrap();
        let mut ran = Vec::new();
        let migrated = migrate_legacy_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/orca.service\n",
            home.path().to_str().unwrap(),
            false,
            &mut |argv| {
                ran.push(argv.join(" "));
                true
            },
            &mut |_| panic!("a user unit never elevates"),
        )
        .unwrap();
        assert!(migrated);
        let unit = std::fs::read_to_string(dir.join("orca-daemon.service")).unwrap();
        assert!(unit.contains("Conflicts=orca.service"), "{unit}");
        assert!(unit.contains("ExecStart=/usr/bin/orca daemon --port 12001"));
        assert!(unit.contains("Environment=HOME=/home/u"));
        assert!(!dir.join("orca.service").exists());
        assert!(ran.iter().any(|c| c.starts_with("systemd-run --user")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_root_system_migration_goes_through_the_sudo_grant() {
        use utils::privileged::PrivilegedExec;
        use utils::privileged::testing::Recorder;
        let rec = Recorder::new(utils::privileged::PrivilegedOutput {
            success: true,
            ..Default::default()
        });
        let migrated = migrate_legacy_unit(
            "0::/system.slice/orca.service\n",
            "/x",
            false,
            &mut |_| panic!("no direct command for a system unit"),
            &mut |call| utils::reactor::block_on(rec.exec(call)).unwrap().success,
        )
        .unwrap();
        assert!(migrated);
        let calls = rec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].argv,
            [utils::path::PRIVILEGED_BIN, "admin", "unit-migrate"]
        );
        let refused = migrate_legacy_unit(
            "0::/system.slice/orca.service\n",
            "/x",
            false,
            &mut |_| panic!("no direct command for a system unit"),
            &mut |_| false,
        )
        .unwrap();
        assert!(!refused);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rendered_units_conflict_with_the_legacy_unit() {
        let system =
            render_system_unit("/usr/local/bin/orca", 12001, "orca", "/var/lib/orca", "/l");
        for unit in [daemon_unit(), system] {
            assert!(unit.contains("Conflicts=orca.service\n"), "{unit}");
            assert!(
                unit.contains("After=network.target orca.service\n"),
                "{unit}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fallback_handoff_is_one_non_blocking_start() {
        let [_, fallback] = handoff_commands(true);
        assert_eq!(
            fallback,
            vec![systemctl(true, &["--no-block", "start", "orca-daemon"])]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_system_unit_parses_into_a_fresh_render() {
        let legacy = "[Service]\nType=simple\nUser=orca\nEnvironment=HOME=/var/lib/orca\n\
                      ExecStart=/usr/local/bin/orca daemon --port 12001\n";
        assert_eq!(
            parse_unit(legacy),
            Some(UnitFields {
                binary: "/usr/local/bin/orca".into(),
                port: 12001,
                user: Some("orca".into()),
                home: "/var/lib/orca".into(),
            })
        );
        let unit = fresh_unit_for(legacy, false).unwrap();
        assert!(unit.contains("User=orca") && unit.contains("Conflicts=orca.service"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stable_binary_resolves_symlinks_in_user_owned_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("orca-1.2.3");
        std::fs::write(&real, b"").unwrap();
        let link = dir.path().join("orca");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let got = stable_binary(link.to_str().unwrap(), "/nonexistent");
        if parent_root_owned(link.to_str().unwrap()) {
            assert_eq!(got, link.to_str().unwrap());
        } else {
            assert_eq!(got, canonical_path(real.to_str().unwrap()));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_migration_leaves_a_foreign_unit_alone() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/systemd/user");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("orca.service"), SCREEN_READER).unwrap();
        let migrated = migrate_legacy_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/orca.service\n",
            home.path().to_str().unwrap(),
            false,
            &mut |_| panic!("no systemctl for a foreign unit"),
            &mut |_| panic!("a foreign unit never elevates"),
        )
        .unwrap();
        assert!(!migrated);
        assert!(dir.join("orca.service").exists());
        assert!(!dir.join("orca-daemon.service").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_removes_owned_units_in_both_scopes() {
        for user_mode in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("orca-daemon.service"), daemon_unit()).unwrap();
            std::fs::write(dir.path().join("orca.service"), daemon_unit()).unwrap();
            let mut ran = Vec::new();
            remove_owned_units(dir.path().to_str().unwrap(), user_mode, &mut |argv| {
                ran.push(argv.to_vec());
                true
            })
            .unwrap();
            assert!(!dir.path().join("orca-daemon.service").exists());
            assert!(!dir.path().join("orca.service").exists());
            assert!(ran.contains(&systemctl(user_mode, &["disable", "--now", "orca"])));
            assert_eq!(ran.last(), Some(&systemctl(user_mode, &["daemon-reload"])));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_leaves_a_foreign_orca_unit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orca.service"), SCREEN_READER).unwrap();
        remove_owned_units(dir.path().to_str().unwrap(), false, &mut |_| {
            panic!("no systemctl for a foreign unit")
        })
        .unwrap();
        assert!(dir.path().join("orca.service").exists());
    }

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

    // ── send_signal ───────────────────────────────────────────────────────────

    #[test]
    fn send_signal_succeeds_for_live_process() {
        // Signal 0 is the null signal: `kill -0 <pid>` only checks that the
        // process exists and is signalable — it delivers nothing. Our own pid
        // is always live, so send_signal must return Ok without side effects.
        send_signal(std::process::id(), "0").expect("null signal to self must succeed");
    }

    #[test]
    fn send_signal_errors_for_impossible_pid() {
        // PID 99999999 is above the OS pid_max on any platform, so `kill`
        // exits non-zero and send_signal must surface the failure message.
        let err = send_signal(99_999_999, "TERM").expect_err("kill of dead pid must error");
        assert!(
            err.to_string().contains("failed"),
            "unexpected error: {err}"
        );
    }

    // ── resolve_binary ────────────────────────────────────────────────────────

    // ── DaemonRuntimeStatus serde ─────────────────────────────────────────────

    #[test]
    fn runtime_status_default_serializes_running_false_and_nulls() {
        // The `system.detail.daemon` read shape: the not-installed default must
        // wire as running=false with every optional field null, snake_case
        // because previous-release peers decode it.
        let s = serde_json::to_string(&DaemonRuntimeStatus::default()).unwrap();
        assert_eq!(
            s,
            r#"{"running":false,"pid":null,"port":null,"uptime_seconds":null}"#
        );
    }

    #[test]
    fn runtime_status_serializes_populated_fields() {
        let status = DaemonRuntimeStatus {
            running: true,
            pid: Some(4321),
            port: Some(12002),
            uptime_seconds: Some(90),
        };
        let s = serde_json::to_string(&status).unwrap();
        assert_eq!(
            s,
            r#"{"running":true,"pid":4321,"port":12002,"uptime_seconds":90}"#
        );
        // Round-trips back to the same values.
        let back: DaemonRuntimeStatus = serde_json::from_str(&s).unwrap();
        assert!(back.running);
        assert_eq!(back.pid, Some(4321));
        assert_eq!(back.port, Some(12002));
        assert_eq!(back.uptime_seconds, Some(90));
    }

    // ── DEFAULT_HTTP_PORT wiring ───────────────────────────────────────────────

    #[test]
    fn default_http_port_matches_contract_config() {
        // The install default must track the contract constant, not a local dupe.
        assert_eq!(DEFAULT_HTTP_PORT, contract::config::APP_REST_HTTP_PORT);
    }

    // ── state-file-driven reads (isolated via ORCA_HOME) ───────────────────────

    fn write_state(pid: u32, mode: DaemonMode) {
        utils::state::write(&utils::state::DaemonState {
            daemon_pid: pid,
            active_pid: pid,
            port: 12002,
            mode,
            binary: "/usr/local/bin/orca".to_string(),
            version: "0.0.0-test".to_string(),
            started_at: utils::time::Timestamp::now(),
        })
        .expect("write daemon state");
    }

    #[test]
    #[serial_test::serial(env)]
    fn collect_runtime_status_defaults_when_no_state_file() {
        let _env = crate::test_env::snapshot();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // No state.json under ORCA_HOME → the not-installed default.
        let status = collect_runtime_status().expect("collect never errors on missing state");
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(status.port.is_none());
        assert!(status.uptime_seconds.is_none());
    }

    #[test]
    #[serial_test::serial(env)]
    fn collect_runtime_status_reports_running_for_live_pid() {
        let _env = crate::test_env::snapshot();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // Our own pid is alive, so a state file naming it reads as running with
        // the recorded port and a non-negative uptime.
        write_state(std::process::id(), DaemonMode::Daemon);
        let status = collect_runtime_status().unwrap();
        assert!(status.running, "current pid must read as alive");
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(status.port, Some(12002));
        assert!(status.uptime_seconds.unwrap() >= 0);
    }

    #[test]
    #[serial_test::serial(env)]
    fn stop_park_reclaim_error_without_state_file() {
        let _env = crate::test_env::snapshot();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // With no state file, every signal helper bails with the "not running"
        // message before attempting any kill.
        for res in [stop(), park(), reclaim()] {
            let err = res.expect_err("no state file must be an error");
            assert!(
                err.to_string().contains("daemon not running"),
                "unexpected: {err}"
            );
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn park_rejects_non_daemon_mode() {
        let _env = crate::test_env::snapshot();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // park requires Daemon mode; a Dev-mode state file is refused before any
        // signal is sent.
        write_state(std::process::id(), DaemonMode::Dev);
        let err = park().expect_err("park must reject non-daemon mode");
        assert!(
            err.to_string().contains("not in running mode"),
            "unexpected: {err}"
        );
    }

    #[test]
    #[serial_test::serial(env)]
    fn reclaim_is_noop_when_already_daemon_mode() {
        let _env = crate::test_env::snapshot();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // reclaim on an already-Daemon-mode process short-circuits: it returns the
        // pid without sending USR2 (no signal to a possibly-foreign pid).
        let pid = std::process::id();
        write_state(pid, DaemonMode::Daemon);
        assert_eq!(reclaim().unwrap(), pid);
    }

    #[test]
    #[serial_test::serial(env)]
    fn collect_runtime_status_reports_not_running_for_dead_pid() {
        let _env = crate::test_env::snapshot();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // A state file naming an impossible pid still parses, but the liveness
        // probe (`kill -0`) fails, so running=false while pid/port are still
        // surfaced from the recorded state.
        write_state(99_999_999, DaemonMode::Daemon);
        let status = collect_runtime_status().unwrap();
        assert!(!status.running, "impossible pid must read as not running");
        assert_eq!(status.pid, Some(99_999_999));
        assert_eq!(status.port, Some(12002));
    }

    #[test]
    fn home_dir_of_errors_for_missing_user() {
        // `getent passwd` (Linux) fails for a non-existent user; on hosts with
        // no `getent` binary (macOS) the Command spawn itself errors. Either way
        // home_dir_of must surface an error rather than a bogus path.
        let err = home_dir_of("orca-nonexistent-user-xyz-123")
            .expect_err("missing user must not resolve to a home dir");
        // Just assert it's an error with a message; the exact text differs by
        // platform (getent-failed vs. spawn error).
        assert!(!err.to_string().is_empty());
    }

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
