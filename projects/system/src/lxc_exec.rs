//! Privileged in-container exec for Proxmox LXC guests — the `pct exec` analogue
//! of the autofs `storage-apply` seam.
//!
//! Managing a Jellyfin (or any) deployment that lives in an LXC means running a
//! command *inside* the container: `apt-get install --only-upgrade jellyfin`,
//! `systemctl restart jellyfin`, etc. On Proxmox that is `pct exec <vmid> -- …`,
//! which needs root/pmxcfs access the non-root orca daemon does not have. Rather
//! than grant the daemon raw `pct` in sudoers (a broad, wildcard privilege), the
//! daemon shells `sudo -n <orca> admin lxc-exec` and the payload rides on stdin —
//! exactly the [`crate::autofs`] `run_privileged` / `execute_privileged` model,
//! so the sudoers grant stays scoped to one orca subcommand with no wildcard.
//!
//! **Defense in depth (root side).** The privileged executor never trusts the
//! op blindly: the target `vmid` must be numeric (it is a `u32`, so this is by
//! construction), `argv[0]` must be a bare name or an exact system path on
//! [`ALLOWED_COMMANDS`] — the small set the deployment-lifecycle tools actually
//! need, and its arguments pass a per-command check that refuses hook and
//! config-injection options and sensitive read paths.
//!
//! Runs **root-side only**, inside the `orca admin lxc-exec` helper behind the
//! `sudo -n` boundary. The daemon-side bridge is [`crate::autofs::run_privileged`]'s
//! sibling; plugins reach it through `plugin_toolkit`'s `lxc_exec` helper (which
//! spawns the same `sudo -n <orca> admin lxc-exec`), never this module directly.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;

use utils::privileged::{LXC_EXEC_HELPER_GRACE, LXC_EXEC_TIMEOUT};

const LXC_UPDATE_TIMEOUT: Duration = Duration::from_secs(3600);

pub const LXC_EXEC_OUTPUT_CAP: usize = 1024 * 1024;

/// Commands the seam permits as `argv[0]`, bare or under `COMMAND_DIRS`. Scoped
/// to the deployment-lifecycle verbs (package upgrade + service restart + a
/// couple of read-only probes). Extend deliberately — every entry widens what a
/// plugin can run as root inside a container.
pub const ALLOWED_COMMANDS: &[&str] = &[
    "apt-get",
    "apt",
    "dpkg",
    "dpkg-query",
    "rpm",
    "systemctl",
    "true", // no-op used by capability preflights
    // Read-only diagnostics for in-guest monitoring (disk usage, file/log peeks).
    "df",
    "cat",
    "ls",
    "stat",
    "head",
    "tail",
    "apk",
    "update",
];

/// A single privileged in-container exec: run `argv` inside LXC `vmid`. Both
/// fields are supplied by the daemon; the root executor validates them before
/// acting. `argv[0]` is the program, the rest its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LxcExecOp {
    /// Target Proxmox LXC vmid.
    pub vmid: u32,
    /// Command + args to run inside the container. `argv[0]` must be on
    /// [`ALLOWED_COMMANDS`].
    pub argv: Vec<String>,
}

/// Outcome of an [`LxcExecOp`]. Mirrors the autofs result shape (errors are
/// collected into the payload, not thrown across the `sudo` boundary): a
/// spawn/validation failure lands in `error` with `exit_code: None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LxcExecResult {
    /// True when the command ran and exited zero.
    pub success: bool,
    /// Process exit code; `None` if it never ran (validation/spawn failure) or
    /// was killed by signal/timeout.
    pub exit_code: Option<i32>,
    /// Captured stdout, lossily decoded; exact bytes are in `stdout_b64`.
    pub stdout: String,
    /// Captured stderr, lossily decoded; exact bytes are in `stderr_b64`.
    pub stderr: String,
    /// Set when the op was refused or could not run; empty on a clean run.
    pub error: String,
    #[serde(default)]
    pub stdout_b64: String,
    #[serde(default)]
    pub stderr_b64: String,
    #[serde(default)]
    pub stdout_len: usize,
    #[serde(default)]
    pub stderr_len: usize,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
}

impl LxcExecResult {
    fn refused(msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            ..Default::default()
        }
    }

    fn from_run(status: std::process::ExitStatus, stdout: Captured, stderr: Captured) -> Self {
        Self {
            success: status.success(),
            exit_code: status.code(),
            stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
            error: String::new(),
            stdout_b64: utils::encoding::base64_encode(&stdout.bytes),
            stderr_b64: utils::encoding::base64_encode(&stderr.bytes),
            stdout_len: stdout.bytes.len(),
            stderr_len: stderr.bytes.len(),
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        }
    }
}

struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_capped<R>(reader: Option<R>, cap: usize) -> std::io::Result<Captured>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return Ok(Captured {
            bytes: Vec::new(),
            truncated: false,
        });
    };
    let mut bytes = Vec::new();
    (&mut reader)
        .take(cap as u64)
        .read_to_end(&mut bytes)
        .await?;
    let extra = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
    Ok(Captured {
        bytes,
        truncated: extra > 0,
    })
}

async fn run_capped(mut cmd: Command, what: &str, timeout: Duration, cap: usize) -> LxcExecResult {
    let child = cmd
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return LxcExecResult::refused(format!("spawn {what}: {e}")),
    };
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let run = async {
        let (out, err, status) =
            tokio::join!(read_capped(out, cap), read_capped(err, cap), child.wait());
        (out, err, status)
    };
    match tokio::time::timeout(timeout, run).await {
        Ok((Ok(out), Ok(err), Ok(status))) => LxcExecResult::from_run(status, out, err),
        Ok((Err(e), _, _)) | Ok((_, Err(e), _)) => {
            LxcExecResult::refused(format!("read {what} output: {e}"))
        }
        Ok((_, _, Err(e))) => LxcExecResult::refused(format!("wait {what}: {e}")),
        Err(_) => {
            _ = child.start_kill();
            LxcExecResult::refused(format!(
                "{what} timed out after {}s — killed",
                timeout.as_secs()
            ))
        }
    }
}

const COMMAND_DIRS: &[&str] = &["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/"];

const APT_SUBCOMMANDS: &[&str] = &[
    "update",
    "upgrade",
    "install",
    "dist-upgrade",
    "full-upgrade",
    "autoremove",
    "remove",
    "purge",
    "clean",
    "autoclean",
    "list",
    "show",
    "policy",
];

const APT_DENIED_LONG: &[&str] = &["--option", "--config-file"];

const DPKG_READONLY_OPTIONS: &[&str] = &[
    "-l",
    "--list",
    "-L",
    "--listfiles",
    "-s",
    "--status",
    "-S",
    "--search",
    "-C",
    "--audit",
    "--get-selections",
    "--print-architecture",
    "--version",
];

const DPKG_DENIED_LONG: &[&str] = &[
    "--pre-invoke",
    "--post-invoke",
    "--root",
    "--admindir",
    "--instdir",
    "--force-script-chrootless",
];

const SYSTEMCTL_SUBCOMMANDS: &[&str] = &[
    "start",
    "stop",
    "restart",
    "reload",
    "try-restart",
    "reload-or-restart",
    "enable",
    "disable",
    "status",
    "is-active",
    "is-enabled",
    "is-failed",
    "show",
    "cat",
    "list-units",
    "list-unit-files",
    "list-timers",
    "daemon-reload",
    "cancel",
    "reset-failed",
];

const SYSTEMCTL_DENIED_LONG: &[&str] = &["--host", "--machine", "--root", "--image"];

const SYSTEMCTL_DENIED_SHORT: &[char] = &['H', 'M'];

const APK_SUBCOMMANDS: &[&str] = &["update", "upgrade"];

const APK_OPTIONS: &[&str] = &["-q", "--quiet", "--no-cache"];

const COMMUNITY_UPDATER: &str = "/usr/bin/update";

const SENSITIVE_PATHS: &[&str] = &[
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/sudoers",
    "/root",
    "/proc",
];

fn resolve_command(prog: &str) -> Result<&'static str, String> {
    let found = if prog.contains('/') {
        ALLOWED_COMMANDS.iter().copied().find(|name| {
            COMMAND_DIRS
                .iter()
                .any(|dir| prog.strip_prefix(dir) == Some(name))
        })
    } else {
        ALLOWED_COMMANDS.iter().copied().find(|name| *name == prog)
    };
    found.ok_or_else(|| {
        format!(
            "refused command '{prog}': not on the lxc-exec allowlist ({})",
            ALLOWED_COMMANDS.join(", ")
        )
    })
}

fn long_name(arg: &str) -> &str {
    arg.split_once('=').map_or(arg, |(name, _)| name)
}

fn is_option(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

fn short_flags(arg: &str) -> Option<&str> {
    arg.strip_prefix('-')
        .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
}

fn refuse(cmd: &str, arg: &str) -> Result<(), String> {
    Err(format!("refused {cmd} argument '{arg}'"))
}

fn check_apt(cmd: &str, args: &[String]) -> Result<(), String> {
    let mut subcommand = false;
    for arg in args {
        if arg.contains("::") {
            return refuse(cmd, arg);
        }
        if let Some(flags) = short_flags(arg) {
            if flags.contains(['o', 'c']) {
                return refuse(cmd, arg);
            }
        } else if is_option(arg) {
            if APT_DENIED_LONG.contains(&long_name(arg)) {
                return refuse(cmd, arg);
            }
        } else if !subcommand {
            if !APT_SUBCOMMANDS.contains(&arg.as_str()) {
                return refuse(cmd, arg);
            }
            subcommand = true;
        } else if arg.contains('/') || arg.ends_with(".deb") {
            return refuse(cmd, arg);
        }
    }
    Ok(())
}

fn check_dpkg(args: &[String]) -> Result<(), String> {
    let mut action = false;
    for arg in args.iter().filter(|arg| is_option(arg)) {
        if DPKG_DENIED_LONG.contains(&long_name(arg))
            || !DPKG_READONLY_OPTIONS.contains(&arg.as_str())
        {
            return refuse("dpkg", arg);
        }
        action = true;
    }
    if action {
        Ok(())
    } else {
        Err("refused dpkg: only read-only query actions are allowed".to_string())
    }
}

fn check_dpkg_query(args: &[String]) -> Result<(), String> {
    match args
        .iter()
        .find(|arg| DPKG_DENIED_LONG.contains(&long_name(arg)))
    {
        Some(arg) => refuse("dpkg-query", arg),
        None => Ok(()),
    }
}

pub const RPM_QA_FORMAT: &str = "%{NAME}\t%{EPOCH}:%{VERSION}-%{RELEASE}\n";

fn check_rpm(args: &[String]) -> Result<(), String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["-qa"] => Ok(()),
        ["-qa", "--qf", format] if *format == RPM_QA_FORMAT => Ok(()),
        _ => Err(format!(
            "refused rpm {}: only `rpm -qa` (optionally with the package-list --qf) is allowed",
            args.join(" ")
        )),
    }
}

fn check_systemctl(args: &[String]) -> Result<(), String> {
    let mut subcommand = false;
    for arg in args {
        if let Some(flags) = short_flags(arg) {
            if flags.contains(SYSTEMCTL_DENIED_SHORT) {
                return refuse("systemctl", arg);
            }
        } else if is_option(arg) {
            if SYSTEMCTL_DENIED_LONG.contains(&long_name(arg)) {
                return refuse("systemctl", arg);
            }
        } else if !subcommand {
            if !SYSTEMCTL_SUBCOMMANDS.contains(&arg.as_str()) {
                return refuse("systemctl", arg);
            }
            subcommand = true;
        }
    }
    Ok(())
}

fn check_apk(args: &[String]) -> Result<(), String> {
    let mut subcommand = false;
    for arg in args {
        if is_option(arg) {
            if !APK_OPTIONS.contains(&arg.as_str()) {
                return refuse("apk", arg);
            }
        } else if subcommand || !APK_SUBCOMMANDS.contains(&arg.as_str()) {
            return refuse("apk", arg);
        } else {
            subcommand = true;
        }
    }
    if subcommand {
        Ok(())
    } else {
        Err("refused apk: needs update or upgrade".to_string())
    }
}

fn check_update(prog: &str, args: &[String]) -> Result<(), String> {
    if prog != COMMUNITY_UPDATER {
        return Err(format!(
            "refused command '{prog}': the updater runs only as {COMMUNITY_UPDATER}"
        ));
    }
    match args.first() {
        Some(arg) => refuse("update", arg),
        None => Ok(()),
    }
}

fn exec_timeout(op: &LxcExecOp) -> Duration {
    match op.argv.first().map(|p| resolve_command(p)) {
        Some(Ok("apk" | "update")) => LXC_UPDATE_TIMEOUT,
        _ => LXC_EXEC_TIMEOUT,
    }
}

fn is_sensitive_path(path: &str) -> bool {
    path.split('/').any(|seg| seg == ".." || seg == ".ssh")
        || SENSITIVE_PATHS.iter().any(|p| {
            path.strip_prefix(p)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '.', '-']))
        })
        || (path.starts_with("/etc/ssh/") && path.contains("_key") && !path.ends_with(".pub"))
}

fn check_read(cmd: &str, args: &[String]) -> Result<(), String> {
    let path = args.iter().find(|arg| {
        !is_option(arg)
            && arg.as_str() != "--"
            && arg.parse::<u64>().is_err()
            && (!arg.starts_with('/') || is_sensitive_path(arg))
    });
    match path {
        Some(arg) => Err(format!(
            "refused {cmd} path '{arg}': must be absolute and not sensitive"
        )),
        None => Ok(()),
    }
}

/// Validate an op against the allowlist without running it. Exposed so the
/// daemon-side bridge (and tests) can reject early with the same rule the root
/// executor enforces.
pub fn validate(op: &LxcExecOp) -> Result<(), String> {
    let prog = op
        .argv
        .first()
        .ok_or_else(|| "empty argv: no command to run".to_string())?;
    let cmd = resolve_command(prog)?;
    let args = &op.argv[1..];
    match cmd {
        "apt-get" | "apt" => check_apt(cmd, args),
        "dpkg" => check_dpkg(args),
        "dpkg-query" => check_dpkg_query(args),
        "rpm" => check_rpm(args),
        "systemctl" => check_systemctl(args),
        "cat" | "head" | "tail" => check_read(cmd, args),
        "apk" => check_apk(args),
        "update" => check_update(prog, args),
        _ => Ok(()),
    }
}

/// Daemon-side bridge to the root helper: spawn `sudo -n <self> admin lxc-exec`
/// and pipe the op as JSON on stdin, returning the parsed [`LxcExecResult`]. The
/// non-root daemon can't run `pct` itself, so it rides the same scoped sudoers
/// grant `orca admin lxc-exec` exposes — the sibling of [`crate::autofs::run_privileged`].
/// A spawn/parse failure surfaces in the result's `error` rather than a panic.
pub async fn run_privileged_lxc(op: &LxcExecOp) -> LxcExecResult {
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return LxcExecResult::refused(format!("serialize op: {e}")),
    };
    let limit = exec_timeout(op) + LXC_EXEC_HELPER_GRACE;
    let call = utils::privileged::PrivilegedCall::helper("lxc-exec", payload, limit);
    match utils::privileged::run(call).await {
        Ok(out) if out.success => serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            LxcExecResult::refused(format!(
                "parse helper output: {e}: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ))
        }),
        Ok(out) => LxcExecResult::refused(format!(
            "helper exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => LxcExecResult::refused(format!(
            "lxc-exec helper did not finish within {}s",
            limit.as_secs()
        )),
        Err(e) => LxcExecResult::refused(format!("run sudo helper: {e}")),
    }
}

/// Execute a validated [`LxcExecOp`] as root via `pct exec`. Called only from the
/// `orca admin lxc-exec` CLI path (behind `sudo -n`). Validates the command
/// allowlist, then runs `pct exec <vmid> -- <argv>` with a bounded timeout,
/// capturing stdout/stderr. Never panics — every failure is returned in the
/// result so the daemon sees a structured outcome across the `sudo` boundary.
pub async fn execute_privileged_lxc(op: LxcExecOp) -> LxcExecResult {
    if let Err(e) = validate(&op) {
        return LxcExecResult::refused(e);
    }
    let mut cmd = match utils::privileged::host_command("pct") {
        Ok(c) => c,
        Err(e) => return LxcExecResult::refused(e.to_string()),
    };

    // `pct exec <vmid> -- <argv...>` — the `--` terminates pct's own option
    // parsing so an argv starting with `-` is passed through to the container.
    let mut pct_argv: Vec<String> = vec!["exec".to_string(), op.vmid.to_string(), "--".to_string()];
    pct_argv.extend(op.argv.iter().cloned());

    cmd.args(&pct_argv);
    run_capped(cmd, "pct exec", exec_timeout(&op), LXC_EXEC_OUTPUT_CAP).await
}

// ── File push (write_file) ────────────────────────────────────────────────────
//
// `write_file` deliberately does NOT ride the exec allowlist: routing file bytes
// through `pct exec` would force allowlisting a shell/`tee`/`dd` and blow the
// scoped seam wide open. Instead this is a separate, confined `pct push` op —
// the daemon hands over `{vmid, path, contents, mode?, owner?}` and the root side
// materializes a host temp file *it alone names* (O_EXCL, 0600, in a root-owned
// state dir), pushes it into the guest, and always unlinks it.

/// Hard cap on pushed file size. A config/secret file is small; a multi-MiB cap
/// keeps a buggy or hostile caller from spilling the host temp dir. Reject over.
const LXC_PUSH_MAX_BYTES: usize = 8 * 1024 * 1024;

/// `pct push` is a local file copy into the container rootfs — fast, but bounded
/// so a wedged push never piles up in D-state.
const LXC_PUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// A single privileged file push: write `contents` to `path` inside LXC `vmid`,
/// applying `mode`/`owner` natively via `pct push`. Supplied by the daemon; the
/// root executor validates before acting. `contents` is redacted from `Debug` so
/// a secret payload never lands in a log line or `{:?}` error.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LxcPushOp {
    /// Target Proxmox LXC vmid.
    pub vmid: u32,
    /// Absolute destination path inside the container.
    pub path: String,
    /// File contents. May carry a secret — redacted from `Debug`.
    pub contents: Vec<u8>,
    /// POSIX mode as an octal string (e.g. `"0640"`). `None` → guest default.
    pub mode: Option<String>,
    /// Owner as `user` or `user:group`. `None` → guest default (root).
    pub owner: Option<String>,
}

impl std::fmt::Debug for LxcPushOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LxcPushOp")
            .field("vmid", &self.vmid)
            .field("path", &self.path)
            .field("contents", &format!("<{} bytes>", self.contents.len()))
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .finish()
    }
}

/// Outcome of an [`LxcPushOp`]. Errors are collected into the payload (not thrown
/// across the `sudo` boundary), mirroring [`LxcExecResult`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LxcPushResult {
    /// True when the file was written into the container.
    pub ok: bool,
    /// Set when the op was refused or `pct push` failed; empty on success.
    pub error: String,
}

impl LxcPushResult {
    fn ok() -> Self {
        Self {
            ok: true,
            error: String::new(),
        }
    }
    fn refused(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: msg.into(),
        }
    }
}

const PUSH_ALLOWED_PREFIXES: &[&str] = &["/etc/", "/opt/", "/srv/", "/var/lib/"];

const PUSH_ALLOWED_FILES: &[&str] = &[
    "/.pxarexclude",
    "/run/orca-update-backup.json",
    "/run/systemd/system/orca-guest-update.service",
];

const PUSH_DENIED_PREFIXES: &[&str] = &[
    "/etc/passwd",
    "/etc/group",
    "/etc/pam.d/",
    "/etc/security/",
    "/etc/ld.so.",
    "/etc/profile",
    "/etc/bash.bashrc",
    "/etc/environment",
    "/etc/cron",
    "/etc/apt/",
    "/etc/dpkg/",
    "/etc/ssh/",
    "/etc/init.d/",
    "/etc/rc",
    "/etc/update-motd.d/",
];

/// Split an `owner` string into `(--user, --group?)` arguments. `"u:g"` →
/// `("u", Some("g"))`, `"u"` → `("u", None)`; empty/`None`/blank → `None` (apply
/// no ownership). Pure so it is unit-testable without a push.
fn split_owner(owner: Option<&str>) -> Option<(String, Option<String>)> {
    let raw = owner?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.split_once(':') {
        Some((u, g)) if !g.trim().is_empty() => {
            Some((u.trim().to_string(), Some(g.trim().to_string())))
        }
        Some((u, _)) => Some((u.trim().to_string(), None)),
        None => Some((raw.to_string(), None)),
    }
}

/// Validate a guest destination path: must be absolute and carry no `..`
/// component (a traversal segment could escape the intended target). The path is
/// the *guest-side* destination `pct push` writes to; keep it well-formed. Pure.
fn validate_push_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("guest path '{path}' is not absolute"));
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err(format!("guest path '{path}' contains a '..' component"));
    }
    if !PUSH_ALLOWED_PREFIXES.iter().any(|p| path.starts_with(p))
        && !PUSH_ALLOWED_FILES.contains(&path)
    {
        return Err(format!(
            "guest path '{path}' is outside the lxc-push allowlist ({})",
            PUSH_ALLOWED_PREFIXES.join(", ")
        ));
    }
    if is_sensitive_path(path) || PUSH_DENIED_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Err(format!("guest path '{path}' is a protected location"));
    }
    Ok(())
}

/// Root-owned directory for host temp files staged before `pct push`. Under
/// orca's own state dir (never /tmp — /tmp is RAM-backed/world-writable on some
/// hosts), created 0700 so only root can see staged content.
fn push_staging_dir() -> Result<std::path::PathBuf, String> {
    let dir = contract::config::paths::state_dir()
        .map_err(|e| format!("resolve orca state dir: {e}"))?
        .join("lxc-push");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create staging dir: {e}"))?;
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|e| format!("chmod staging dir: {e}"))?;
    Ok(dir)
}

/// Removes its path on drop so an early return still unlinks the host temp file.
struct TempFileGuard(std::path::PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            tracing::warn!(path = %self.0.display(), error = %e, "unlink lxc-push host temp");
        }
    }
}

/// Daemon-side bridge: spawn `sudo -n <self> admin lxc-push` and pipe the op as
/// JSON on stdin, returning the parsed [`LxcPushResult`]. Exact sibling of
/// [`run_privileged_lxc`]; a spawn/parse failure surfaces in `error`, never a
/// panic. The op's `contents` ride stdin, never argv or a log line.
pub async fn run_privileged_lxc_push(op: &LxcPushOp) -> LxcPushResult {
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return LxcPushResult::refused(format!("serialize op: {e}")),
    };
    let call = utils::privileged::PrivilegedCall::helper(
        "lxc-push",
        payload,
        utils::privileged::LXC_PUSH_HELPER_TIMEOUT,
    );
    match utils::privileged::run(call).await {
        Ok(out) if out.success => serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            LxcPushResult::refused(format!(
                "parse helper output: {e}: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ))
        }),
        Ok(out) => LxcPushResult::refused(format!(
            "helper exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => LxcPushResult::refused(format!("run sudo helper: {e}")),
    }
}

async fn guest_path_is_symlink(vmid: u32, path: &str) -> bool {
    let Ok(mut cmd) = utils::privileged::host_command("pct") else {
        return true;
    };
    let probe = cmd
        .args([
            "exec",
            &vmid.to_string(),
            "--",
            "stat",
            "-c",
            "%F",
            "--",
            path,
        ])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(LXC_PUSH_TIMEOUT, probe).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).trim() == "symbolic link",
        _ => false,
    }
}

/// Execute an [`LxcPushOp`] as root via `pct push`. Called only from the
/// `orca admin lxc-push` CLI path (behind `sudo -n`). SAFETY-CRITICAL: the host
/// staging path is chosen entirely root-side (O_EXCL + 0600 in a root-owned dir),
/// never caller-supplied — a predictable/influenced host path would be a root
/// arbitrary-write/TOCTOU vector. The temp file is always unlinked. Never panics.
pub async fn execute_privileged_lxc_push(op: LxcPushOp) -> LxcPushResult {
    if let Err(e) = validate_push_path(&op.path) {
        return LxcPushResult::refused(e);
    }
    if op.contents.len() > LXC_PUSH_MAX_BYTES {
        return LxcPushResult::refused(format!(
            "contents {} bytes exceeds the {LXC_PUSH_MAX_BYTES}-byte push cap",
            op.contents.len()
        ));
    }
    let mut push = match utils::privileged::host_command("pct") {
        Ok(c) => c,
        Err(e) => return LxcPushResult::refused(e.to_string()),
    };

    if guest_path_is_symlink(op.vmid, &op.path).await {
        return LxcPushResult::refused(format!(
            "guest path '{}' is a symlink; refusing to follow it",
            op.path
        ));
    }

    let dir = match push_staging_dir() {
        Ok(d) => d,
        Err(e) => return LxcPushResult::refused(e),
    };

    // Root-chosen host temp path: unpredictable name, created O_EXCL so we never
    // follow or clobber a pre-existing path, mode 0600 so only root can read it.
    let name = format!(
        "push-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let host_tmp = dir.join(name);

    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&host_tmp)
        {
            Ok(f) => f,
            Err(e) => return LxcPushResult::refused(format!("create host temp file: {e}")),
        };
        // Guard is armed the moment the file exists, so every path below unlinks it.
        let _guard = TempFileGuard(host_tmp.clone());
        if let Err(e) = f.write_all(&op.contents).and_then(|()| f.flush()) {
            return LxcPushResult::refused(format!("write host temp file: {e}"));
        }
        drop(f);

        let mut argv: Vec<String> = vec![
            "push".to_string(),
            op.vmid.to_string(),
            host_tmp.to_string_lossy().into_owned(),
            op.path.clone(),
        ];
        if let Some(mode) = op.mode.as_deref().filter(|m| !m.trim().is_empty()) {
            argv.push("--perms".to_string());
            argv.push(mode.trim().to_string());
        }
        if let Some((user, group)) = split_owner(op.owner.as_deref()) {
            argv.push("--user".to_string());
            argv.push(user);
            if let Some(group) = group {
                argv.push("--group".to_string());
                argv.push(group);
            }
        }

        let child = push
            .args(&argv)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return LxcPushResult::refused(format!("spawn pct: {e}")),
        };

        match tokio::time::timeout(LXC_PUSH_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => LxcPushResult::ok(),
            Ok(Ok(out)) => LxcPushResult::refused(format!(
                "pct push exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(Err(e)) => LxcPushResult::refused(format!("wait pct: {e}")),
            Err(_) => LxcPushResult::refused(format!(
                "pct push timed out after {}s — killed",
                LXC_PUSH_TIMEOUT.as_secs()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[tokio::test]
    async fn output_is_returned_byte_exact() {
        let r = run_capped(
            sh(r"printf '  a\377b\n\n'; printf 'e \n' >&2; exit 3"),
            "sh",
            Duration::from_secs(10),
            LXC_EXEC_OUTPUT_CAP,
        )
        .await;
        assert_eq!(r.exit_code, Some(3), "{r:?}");
        let out = utils::encoding::base64_decode(&r.stdout_b64).unwrap();
        assert_eq!(out, b"  a\xffb\n\n");
        assert_eq!(r.stdout_len, out.len());
        assert_eq!(
            utils::encoding::base64_decode(&r.stderr_b64).unwrap(),
            b"e \n"
        );
        assert!(!r.stdout_truncated && !r.stderr_truncated);
        assert_eq!(r.stderr, "e \n");
    }

    #[tokio::test]
    async fn output_is_capped_and_flagged() {
        let r = run_capped(
            sh("head -c 5000 /dev/zero; head -c 10 /dev/zero >&2"),
            "sh",
            Duration::from_secs(10),
            1024,
        )
        .await;
        assert!(r.success, "{r:?}");
        assert_eq!(r.stdout_len, 1024);
        assert!(r.stdout_truncated);
        assert_eq!(r.stderr_len, 10);
        assert!(!r.stderr_truncated);
    }

    #[tokio::test]
    async fn a_timeout_kills_the_child() {
        let started = std::time::Instant::now();
        let r = run_capped(sh("sleep 30"), "sh", Duration::from_millis(200), 1024).await;
        assert!(r.error.contains("timed out"), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dropping_the_call_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("done");
        let script = format!("sleep 1; touch {}", marker.display());
        let call = run_capped(sh(&script), "sh", Duration::from_secs(10), 1024);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), call)
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists());
    }

    #[test]
    fn allowlist_accepts_bare_and_absolute() {
        assert!(
            validate(&LxcExecOp {
                vmid: 113,
                argv: vec!["apt-get".into(), "update".into()],
            })
            .is_ok()
        );
        assert!(
            validate(&LxcExecOp {
                vmid: 113,
                argv: vec![
                    "/usr/bin/systemctl".into(),
                    "restart".into(),
                    "jellyfin".into()
                ],
            })
            .is_ok()
        );
    }

    #[test]
    fn allowlist_accepts_readonly_diagnostics() {
        // Read-only probes added to unblock in-guest disk/log monitoring.
        for cmd in ["df", "cat", "ls", "stat", "head", "tail"] {
            assert!(
                validate(&LxcExecOp {
                    vmid: 100,
                    argv: vec![cmd.into(), "/etc/os-release".into()],
                })
                .is_ok(),
                "{cmd} should be allowlisted"
            );
        }
        // A clearly-destructive command stays rejected.
        assert!(
            validate(&LxcExecOp {
                vmid: 100,
                argv: vec!["rm".into(), "-rf".into(), "/".into()],
            })
            .is_err()
        );
    }

    #[test]
    fn allowlist_rejects_arbitrary_and_empty() {
        let err = validate(&LxcExecOp {
            vmid: 113,
            argv: vec!["/bin/sh".into(), "-c".into(), "rm -rf /".into()],
        })
        .unwrap_err();
        assert!(err.contains("not on the lxc-exec allowlist"), "{err}");

        let err = validate(&LxcExecOp {
            vmid: 113,
            argv: vec![],
        })
        .unwrap_err();
        assert!(err.contains("empty argv"), "{err}");
    }

    fn check(argv: &[&str]) -> Result<(), String> {
        validate(&LxcExecOp {
            vmid: 100,
            argv: argv.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn legit_callers_still_pass() {
        for argv in [
            &["apt-get", "update"][..],
            &[
                "apt-get",
                "install",
                "-y",
                "--only-upgrade",
                "jellyfin",
                "jellyfin-server",
            ],
            &["apt-get", "upgrade", "-y"],
            &["systemctl", "restart", "jellyfin"],
            &[
                "systemctl",
                "stop",
                "--job-mode=fail",
                "orca-guest-update.service",
            ],
            &["systemctl", "show", "-p", "ActiveState", "x.service"],
            &["systemctl", "daemon-reload"],
            &["head", "-c", "65536", "--", "/etc/os-release"],
            &["tail", "-n", "50", "/var/log/syslog"],
            &["stat", "-L", "-c", "%F|%s", "--", "/etc/inittab"],
            &["dpkg-query", "-W", "jellyfin"],
            &["dpkg", "-l"],
            &["/usr/bin/systemctl", "restart", "jellyfin"],
            &["/bin/cat", "/etc/hostname"],
            &["true"],
        ] {
            assert!(check(argv).is_ok(), "{argv:?}: {:?}", check(argv));
        }
    }

    #[test]
    fn argv0_must_be_exact_system_path() {
        assert!(check(&["/tmp/apt-get", "update"]).is_err());
        assert!(check(&["/usr/local/bin/cat", "/etc/hostname"]).is_err());
        assert!(check(&["./cat", "/etc/hostname"]).is_err());
        assert!(check(&["/usr/bin/../../tmp/cat", "/etc/hostname"]).is_err());
    }

    #[test]
    fn apt_option_injection_refused() {
        assert!(check(&["apt-get", "-o", "APT::Update::Pre-Invoke::=id", "update"]).is_err());
        assert!(check(&["apt-get", "-oAPT::Update::Pre-Invoke::=id", "update"]).is_err());
        assert!(check(&["apt-get", "-yo", "x", "update"]).is_err());
        assert!(check(&["apt-get", "--option=Dpkg::Pre-Invoke::=id", "update"]).is_err());
        assert!(check(&["apt", "-c", "/tmp/evil.conf", "update"]).is_err());
        assert!(check(&["apt-get", "--config-file", "/tmp/evil.conf", "update"]).is_err());
        assert!(check(&["apt-get", "install", "/tmp/evil.deb"]).is_err());
        assert!(check(&["apt-get", "install", "./evil.deb"]).is_err());
        assert!(check(&["apt-get", "source", "jellyfin"]).is_err());
    }

    #[test]
    fn dpkg_hooks_and_installs_refused() {
        assert!(check(&["dpkg", "--pre-invoke=id", "-l"]).is_err());
        assert!(check(&["dpkg", "--post-invoke", "id", "-l"]).is_err());
        assert!(check(&["dpkg", "--root=/tmp/x", "-l"]).is_err());
        assert!(check(&["dpkg", "--admindir", "/tmp/x", "-l"]).is_err());
        assert!(check(&["dpkg", "--instdir=/tmp/x", "-l"]).is_err());
        assert!(check(&["dpkg", "--force-script-chrootless", "-l"]).is_err());
        assert!(check(&["dpkg", "-i", "/tmp/evil.deb"]).is_err());
        assert!(check(&["dpkg", "jellyfin"]).is_err());
        assert!(check(&["dpkg-query", "--admindir=/tmp/x", "-W"]).is_err());
    }

    #[test]
    fn rpm_is_limited_to_listing_installed_packages() {
        assert!(check(&["rpm", "-qa"]).is_ok());
        assert!(check(&["/usr/bin/rpm", "-qa"]).is_ok());
        assert!(check(&["rpm"]).is_err());
        assert!(check(&["rpm", "-i", "/tmp/evil.rpm"]).is_err());
        assert!(check(&["rpm", "-qa", "--qf", RPM_QA_FORMAT]).is_ok());
        assert!(check(&["rpm", "-qa", "--qf", "%{NAME}"]).is_err());
        assert!(check(&["rpm", "-qa", "--dbpath", "/tmp/x"]).is_err());
        assert!(check(&["rpm", "--eval", "%(id)"]).is_err());
    }

    #[test]
    fn systemctl_remote_and_unknown_verbs_refused() {
        assert!(check(&["systemctl", "-H", "host", "restart", "x"]).is_err());
        assert!(check(&["systemctl", "--machine=other", "restart", "x"]).is_err());
        assert!(check(&["systemctl", "--root=/tmp", "enable", "x"]).is_err());
        assert!(check(&["systemctl", "edit", "x"]).is_err());
        assert!(check(&["systemctl", "set-environment", "LD_PRELOAD=/tmp/x"]).is_err());
    }

    #[test]
    fn sensitive_reads_refused() {
        for path in [
            "/etc/shadow",
            "/etc/gshadow",
            "/etc/shadow-",
            "/etc/sudoers",
            "/etc/sudoers.d/orca",
            "/root/.bash_history",
            "/home/u/.ssh/id_ed25519",
            "/etc/ssh/ssh_host_ed25519_key",
            "/proc/1/environ",
            "/etc/../etc/shadow",
            "etc/shadow",
        ] {
            for cmd in ["cat", "head", "tail"] {
                assert!(check(&[cmd, path]).is_err(), "{cmd} {path}");
            }
        }
        assert!(check(&["head", "-c", "10", "--", "/etc/shadow"]).is_err());
        assert!(check(&["cat", "/etc/ssh/ssh_host_ed25519_key.pub"]).is_ok());
    }

    #[test]
    fn apk_allows_only_index_refresh_and_upgrade() {
        for argv in [
            &["apk", "update"][..],
            &["apk", "upgrade"],
            &["/sbin/apk", "update", "-q"],
            &["apk", "-q", "--no-cache", "upgrade"],
        ] {
            assert!(check(argv).is_ok(), "{argv:?}: {:?}", check(argv));
        }
        for argv in [
            &["apk"][..],
            &["apk", "-q"],
            &["apk", "add", "curl"],
            &["apk", "del", "openrc"],
            &["apk", "upgrade", "busybox"],
            &["apk", "update", "upgrade"],
            &["apk", "--root", "/tmp/x", "upgrade"],
            &["apk", "--repository=http://evil/", "upgrade"],
            &["apk", "-X", "http://evil/", "upgrade"],
            &["apk", "--allow-untrusted", "upgrade"],
            &["/tmp/apk", "update"],
        ] {
            assert!(check(argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn community_updater_runs_only_from_its_path_without_args() {
        assert!(check(&["/usr/bin/update"]).is_ok());
        assert!(check(&["update"]).is_err());
        assert!(check(&["/bin/update"]).is_err());
        assert!(check(&["/usr/sbin/update"]).is_err());
        assert!(check(&["/usr/local/bin/update"]).is_err());
        assert!(check(&["/usr/bin/update", "--force"]).is_err());
    }

    #[test]
    fn updaters_get_the_long_timeout() {
        let op = |argv: &[&str]| LxcExecOp {
            vmid: 100,
            argv: argv.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(exec_timeout(&op(&["apk", "upgrade"])), LXC_UPDATE_TIMEOUT);
        assert_eq!(
            exec_timeout(&op(&["/sbin/apk", "update"])),
            LXC_UPDATE_TIMEOUT
        );
        assert_eq!(exec_timeout(&op(&["/usr/bin/update"])), LXC_UPDATE_TIMEOUT);
        assert_eq!(exec_timeout(&op(&["apt-get", "update"])), LXC_EXEC_TIMEOUT);
        assert_eq!(exec_timeout(&op(&["curl"])), LXC_EXEC_TIMEOUT);
        assert_eq!(exec_timeout(&op(&[])), LXC_EXEC_TIMEOUT);
    }

    #[test]
    fn push_destinations_restricted() {
        assert!(validate_push_path("/etc/systemd/system/x.service.d/autologin.conf").is_ok());
        assert!(validate_push_path("/etc/inittab").is_ok());
        assert!(validate_push_path("/.pxarexclude").is_ok());
        assert!(validate_push_path("/var/lib/app/config.yml").is_ok());
        assert!(validate_push_path("/run/orca-update-backup.json").is_ok());
        assert!(validate_push_path("/run/systemd/system/orca-guest-update.service").is_ok());
        for path in [
            "/usr/local/bin/apt-get",
            "/usr/bin/systemctl",
            "/bin/sh",
            "/tmp/x",
            "/root/.ssh/authorized_keys",
            "/home/u/.ssh/authorized_keys",
            "/etc/sudoers.d/evil",
            "/etc/shadow",
            "/etc/passwd",
            "/etc/ld.so.preload",
            "/etc/pam.d/common-auth",
            "/etc/cron.d/evil",
            "/etc/apt/apt.conf.d/99evil",
            "/etc/profile.d/evil.sh",
            "/etc/ssh/sshd_config",
            "/run/systemd/system/evil.service",
            "/run/orca-update-backup.json.d/x",
        ] {
            assert!(validate_push_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn split_owner_variants() {
        assert_eq!(
            split_owner(Some("root:root")),
            Some(("root".to_string(), Some("root".to_string())))
        );
        assert_eq!(split_owner(Some("root")), Some(("root".to_string(), None)));
        // A trailing colon with no group applies just the user.
        assert_eq!(split_owner(Some("root:")), Some(("root".to_string(), None)));
        assert_eq!(split_owner(Some("")), None);
        assert_eq!(split_owner(Some("   ")), None);
        assert_eq!(split_owner(None), None);
    }

    #[test]
    fn validate_push_path_rules() {
        assert!(validate_push_path("/etc/example.conf").is_ok());
        assert!(validate_push_path("etc/example.conf").is_err());
        assert!(validate_push_path("../etc/example.conf").is_err());
        assert!(validate_push_path("/etc/../../root/.ssh/authorized_keys").is_err());
    }

    #[test]
    fn push_op_debug_redacts_contents() {
        let op = LxcPushOp {
            vmid: 100,
            path: "/etc/example.conf".into(),
            contents: b"password=hunter2".to_vec(),
            mode: Some("0640".into()),
            owner: Some("root:root".into()),
        };
        let dbg = format!("{op:?}");
        assert!(!dbg.contains("hunter2"), "contents must be redacted");
        assert!(dbg.contains("<16 bytes>"));
    }

    #[tokio::test]
    async fn push_rejects_relative_path_before_pct() {
        let res = execute_privileged_lxc_push(LxcPushOp {
            vmid: 100,
            path: "etc/example.conf".into(),
            contents: b"x".to_vec(),
            mode: None,
            owner: None,
        })
        .await;
        assert!(!res.ok);
        assert!(res.error.contains("not absolute"), "{}", res.error);
    }

    #[tokio::test]
    async fn refused_command_never_spawns_pct() {
        // A non-allowlisted command must be refused before any pct spawn, so this
        // returns a clean refusal even on a host with no pct.
        let res = execute_privileged_lxc(LxcExecOp {
            vmid: 113,
            argv: vec!["curl".into(), "evil.example".into()],
        })
        .await;
        assert!(!res.success);
        assert!(
            res.error.contains("not on the lxc-exec allowlist"),
            "{}",
            res.error
        );
        assert_eq!(res.exit_code, None);
    }

    #[tokio::test]
    async fn an_allowlisted_command_never_spawns_pct_under_test() {
        let res = execute_privileged_lxc(LxcExecOp {
            vmid: 113,
            argv: vec!["true".into()],
        })
        .await;
        assert!(!res.success);
        assert!(
            res.error.contains("refusing to run host-mutating `pct`"),
            "{}",
            res.error
        );
    }

    #[tokio::test]
    async fn a_valid_push_never_spawns_pct_under_test() {
        let res = execute_privileged_lxc_push(LxcPushOp {
            vmid: 113,
            path: "/etc/orca-test".into(),
            contents: b"x".to_vec(),
            mode: None,
            owner: None,
        })
        .await;
        assert!(!res.ok);
        assert!(
            res.error.contains("refusing to run host-mutating `pct`"),
            "{}",
            res.error
        );
    }
}
