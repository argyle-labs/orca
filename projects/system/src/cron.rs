//! `system.cron.*` — read and edit a user's crontab on this host.
//!
//! Every target must be an existing user with a non-zero uid, checked before
//! either path is chosen. The daemon's own crontab is read and installed with
//! plain `crontab -l` / `crontab -` (refused if the daemon itself runs as
//! root). Any other user's crontab needs root, so it rides the scoped
//! `sudo -n <orca> admin cron-apply` helper with a JSON [`CronOp`] on stdin — the
//! [`crate::lxc_exec`] model. The root side:
//!
//! - only runs `crontab -u <user> -l` or `crontab -u <user> -`, never touching
//!   the spool directly;
//! - serves only users named in the root-owned allow-list [`CRON_USERS_ALLOWLIST`]
//!   (one login per line, `#` comments), opened without following symlinks
//!   and trusted only when it, `/etc/orca` and `/etc` are root-owned and not
//!   group/world-writable; uid 0 is refused even when listed;
//! - re-checks the expected hash, installs, reads back, and reinstalls the
//!   previous text if the read-back differs — all in one invocation under one
//!   40s deadline. `crontab` takes no lock, so a concurrent `crontab -e` by the
//!   user can still land between the compare and the write; this only narrows
//!   that window;
//! - refuses a crontab that is not UTF-8.
//!
//! The sudoers grant for the helper is only installed when the allow-list
//! exists (see `sysadmin::install_autofs_sudoers`).
//!
//! `system.cron.diff` plans an edit and returns the before/after diff plus the
//! hash it planned against; `system.cron.update` applies it (dry-run unless
//! `execute`). All three verbs are admin-only: crontabs routinely carry secrets.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::process::{Child, Command};

/// Root-owned list of users other than the daemon's own whose crontab the
/// helper may read or install.
pub const CRON_USERS_ALLOWLIST: &str = "/etc/orca/cron-users";

/// Per `crontab` invocation.
const CRONTAB_TIMEOUT: Duration = Duration::from_secs(30);
/// One budget for a whole op (read, install, read-back, rollback), shorter
/// than [`HELPER_TIMEOUT`] so the helper always answers before the bridge
/// gives up on it.
const OP_DEADLINE: Duration = Duration::from_secs(40);
const HELPER_TIMEOUT: Duration = Duration::from_secs(45);
/// Raw `crontab`/helper stderr kept in an error.
const STDERR_SNIPPET_BYTES: usize = 512;
const TERM_GRACE: Duration = Duration::from_secs(3);

pub const CRONTAB_MAX_BYTES: usize = 256 * 1024;
/// Cap on the helper's stdin: the crontab plus JSON framing.
pub const CRON_OP_MAX_BYTES: usize = CRONTAB_MAX_BYTES + 4096;

const SCHEDULE_KEYWORDS: &[&str] = &[
    "@reboot",
    "@yearly",
    "@annually",
    "@monthly",
    "@weekly",
    "@daily",
    "@midnight",
    "@hourly",
];

// ── privileged seam ──────────────────────────────────────────────────────────

/// One crontab operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CronOp {
    /// Read the crontab.
    Read { user: String },
    /// Install `text` if the live crontab still hashes to `expected_sha`.
    Write {
        user: String,
        text: String,
        expected_sha: String,
    },
}

impl CronOp {
    pub fn user(&self) -> &str {
        match self {
            CronOp::Read { user } | CronOp::Write { user, .. } => user,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            CronOp::Read { .. } => "read",
            CronOp::Write { .. } => "write",
        }
    }
}

/// Outcome of a [`CronOp`]; failures land in `error`, never thrown across
/// `sudo`. Errors carry hashes and lengths, never crontab text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronOpResult {
    pub success: bool,
    /// The crontab text for `read` (empty when the user has none).
    pub text: String,
    pub error: String,
}

impl CronOpResult {
    fn ok(text: String) -> Self {
        Self {
            success: true,
            text,
            error: String::new(),
        }
    }

    pub fn refused(error: impl Into<String>) -> Self {
        Self {
            success: false,
            text: String::new(),
            error: error.into(),
        }
    }
}

/// POSIX-portable login name: no whitespace, quoting, path or option characters
/// can reach `crontab -u`.
pub fn validate_user(user: &str) -> Result<(), String> {
    let mut chars = user.chars();
    let ok_first = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    let ok_rest = user
        .chars()
        .skip(1)
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'));
    if ok_first && ok_rest && user.len() <= 32 {
        Ok(())
    } else {
        Err(format!(
            "invalid user name '{user}': expected [a-z_][a-z0-9_.-]{{0,31}}"
        ))
    }
}

/// uid of `name` via `getpwnam_r` (honours NSS, unlike parsing `/etc/passwd`).
#[cfg(unix)]
pub fn lookup_uid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call; `result` aliases `pwd` on success.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    (rc == 0 && !result.is_null()).then_some(pwd.pw_uid)
}

/// Login name of the effective uid this process runs as.
#[cfg(unix)]
pub fn current_user() -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: as in `lookup_uid`; `pw_name` points into `buf`, read before it drops.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    name.to_str().ok().map(str::to_string)
}

/// Logins in the allow-list text: one per line, `#` comments and blanks ignored.
pub fn parse_allowlist(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The allow-list (and each directory above it) is only trusted when root owns
/// it and nobody else can write it.
pub fn check_root_only_writable(path: &str, owner_uid: u32, mode: u32) -> Result<(), String> {
    if owner_uid != 0 {
        return Err(format!(
            "{path} is owned by uid {owner_uid}, not root; refusing to trust {CRON_USERS_ALLOWLIST}"
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "{path} is group/world-writable (mode {:o}); refusing to trust {CRON_USERS_ALLOWLIST}",
            mode & 0o7777
        ));
    }
    Ok(())
}

/// The uid of a target `user`, which must exist and must not be root. Both the
/// direct and the sudo path call this before doing anything else.
pub fn target_uid(user: &str, uid: Option<u32>) -> Result<u32, String> {
    match uid {
        None => Err(format!("no such user '{user}'")),
        Some(0) => Err(format!(
            "refusing '{user}': uid 0 crontabs are never managed"
        )),
        Some(uid) => Ok(uid),
    }
}

/// The direct (no-sudo) path is for an unprivileged daemon's own crontab; a
/// daemon running as root goes nowhere near it.
pub fn direct_path_allowed(euid: u32) -> Result<(), String> {
    if euid == 0 {
        return Err(
            "refusing: orca is running as root; crontabs are only managed \
                    for an unprivileged daemon"
                .into(),
        );
    }
    Ok(())
}

/// Whether the root helper may touch `user`'s crontab.
pub fn authorize_target(user: &str, uid: u32, allowlist: &[String]) -> Result<(), String> {
    if uid == 0 {
        return Err(format!(
            "refusing '{user}': uid 0 crontabs are never managed"
        ));
    }
    if !allowlist.iter().any(|u| u == user) {
        let kind = if uid < 1000 { "system account" } else { "user" };
        return Err(format!(
            "refusing {kind} '{user}' (uid {uid}): not listed in {CRON_USERS_ALLOWLIST}"
        ));
    }
    Ok(())
}

/// Open once without following a symlink, check owner/mode on that fd, and read
/// from the same fd, so the file checked is the file read.
#[cfg(unix)]
fn load_allowlist() -> Result<Vec<String>, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = std::path::Path::new(CRON_USERS_ALLOWLIST);
    for dir in path
        .ancestors()
        .skip(1)
        .filter(|d| *d != std::path::Path::new("/"))
    {
        let shown = dir.display().to_string();
        let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("stat {shown}: {e}"))?;
        if !meta.is_dir() {
            return Err(format!("{shown} is not a directory; refusing to trust it"));
        }
        check_root_only_writable(&shown, meta.uid(), meta.mode())?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("open {CRON_USERS_ALLOWLIST}: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("fstat {CRON_USERS_ALLOWLIST}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{CRON_USERS_ALLOWLIST} is not a regular file"));
    }
    check_root_only_writable(CRON_USERS_ALLOWLIST, meta.uid(), meta.mode())?;
    let mut text = String::new();
    file.take(64 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| format!("read {CRON_USERS_ALLOWLIST}: {e}"))?;
    Ok(parse_allowlist(&text))
}

fn stderr_snippet(stderr: &[u8]) -> String {
    let cut = &stderr[..stderr.len().min(STDERR_SNIPPET_BYTES)];
    let mut out = String::from_utf8_lossy(cut).trim().to_string();
    if stderr.len() > STDERR_SNIPPET_BYTES {
        out.push_str(" [truncated]");
    }
    out
}

/// Time left before `deadline`, capped at one `crontab` call's budget.
fn step_limit(deadline: tokio::time::Instant) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    if left.is_zero() {
        return Err(format!(
            "op deadline of {}s exceeded",
            OP_DEADLINE.as_secs()
        ));
    }
    Ok(left.min(CRONTAB_TIMEOUT))
}

/// Wait for `child`, sending SIGTERM at `limit` (which sudo relays to its
/// child) and SIGKILL after a short grace.
async fn wait_bounded(child: Child, limit: Duration) -> Result<std::process::Output, String> {
    let pid = child.id();
    let mut task = tokio::spawn(child.wait_with_output());
    match tokio::time::timeout(limit, &mut task).await {
        Ok(Ok(res)) => return res.map_err(|e| format!("wait: {e}")),
        Ok(Err(e)) => return Err(format!("wait task: {e}")),
        Err(_) => {}
    }
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: plain signal send to a pid we spawned and have not reaped.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(TERM_GRACE, &mut task).await.is_err() {
        // Dropping the child inside the aborted task SIGKILLs it (kill_on_drop).
        task.abort();
    }
    Err(format!("timed out after {}s", limit.as_secs()))
}

/// Daemon entry point: the daemon's own crontab directly, anyone else's through
/// the root helper.
pub async fn run_cron(op: &CronOp) -> CronOpResult {
    if let Err(e) = validate_user(op.user()) {
        return CronOpResult::refused(e);
    }
    #[cfg(unix)]
    {
        if let Err(e) = target_uid(op.user(), lookup_uid(op.user())) {
            return CronOpResult::refused(e);
        }
        if current_user().as_deref() == Some(op.user()) {
            // SAFETY: geteuid has no preconditions.
            if let Err(e) = direct_path_allowed(unsafe { libc::geteuid() }) {
                return CronOpResult::refused(e);
            }
            return apply_op(None, op).await;
        }
    }
    run_privileged_cron(op).await
}

/// Spawn `sudo -n <self> admin cron-apply` with `op` on stdin.
async fn run_privileged_cron(op: &CronOp) -> CronOpResult {
    use tokio::io::AsyncWriteExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return CronOpResult::refused(format!("resolve current exe: {e}")),
    };
    let payload = match serde_json::to_vec(op) {
        Ok(v) => v,
        Err(e) => return CronOpResult::refused(format!("serialize op: {e}")),
    };

    let mut child = match Command::new("sudo")
        .arg("-n")
        .arg(&exe)
        .args(["admin", "cron-apply"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return CronOpResult::refused(format!("spawn sudo helper: {e}")),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _written = stdin.write_all(&payload).await;
        let _shut = stdin.shutdown().await;
    }

    match wait_bounded(child, HELPER_TIMEOUT).await {
        Ok(out) if out.status.success() => {
            serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
                CronOpResult::refused(format!(
                    "parse helper output ({} bytes): {e}",
                    out.stdout.len()
                ))
            })
        }
        Ok(out) => CronOpResult::refused(format!(
            "helper exit {}: {}. Is `{CRON_USERS_ALLOWLIST}` present and the \
             `orca admin cron-apply` sudoers grant installed? (re-run `orca system install`)",
            out.status,
            stderr_snippet(&out.stderr)
        )),
        Err(e) => CronOpResult::refused(format!("cron-apply helper: {e}")),
    }
}

/// Root side of `orca admin cron-apply`. Re-validates everything the daemon sent.
pub async fn execute_privileged_cron(op: CronOp) -> CronOpResult {
    let user = op.user().to_string();
    if let Err(e) = validate_user(&user) {
        return CronOpResult::refused(e);
    }
    #[cfg(unix)]
    {
        let uid = match target_uid(&user, lookup_uid(&user)) {
            Ok(uid) => uid,
            Err(e) => return CronOpResult::refused(e),
        };
        let allowlist = match load_allowlist() {
            Ok(a) => a,
            Err(e) => return CronOpResult::refused(e),
        };
        if let Err(e) = authorize_target(&user, uid, &allowlist) {
            return CronOpResult::refused(e);
        }
    }
    apply_op(Some(&user), &op).await
}

/// Run `op` against `target`'s crontab (`-u target`), or the caller's own when
/// `None`.
async fn apply_op(target: Option<&str>, op: &CronOp) -> CronOpResult {
    let deadline = tokio::time::Instant::now() + OP_DEADLINE;
    match op {
        CronOp::Read { .. } => match crontab_read(target, deadline).await {
            Ok(text) => CronOpResult::ok(text),
            Err(e) => CronOpResult::refused(e),
        },
        CronOp::Write {
            text, expected_sha, ..
        } => write_checked(target, text, expected_sha, deadline).await,
    }
}

fn reject_bad_text(text: &str) -> Result<(), String> {
    if text.len() > CRONTAB_MAX_BYTES {
        return Err(format!(
            "crontab is {} bytes; cap is {CRONTAB_MAX_BYTES}",
            text.len()
        ));
    }
    if text.contains('\0') {
        return Err("crontab text contains a NUL byte".into());
    }
    Ok(())
}

async fn write_checked(
    target: Option<&str>,
    text: &str,
    expected_sha: &str,
    deadline: tokio::time::Instant,
) -> CronOpResult {
    if let Err(e) = reject_bad_text(text) {
        return CronOpResult::refused(e);
    }
    let before = match crontab_read(target, deadline).await {
        Ok(t) => t,
        Err(e) => return CronOpResult::refused(e),
    };
    let before_sha = content_hash(&before);
    if before_sha != expected_sha {
        return CronOpResult::refused(format!(
            "crontab changed since it was planned (expected {expected_sha}, now {before_sha}); \
             re-run system.cron.diff"
        ));
    }
    if let Err(e) = crontab_install(target, text, deadline).await {
        return CronOpResult::refused(e);
    }
    let readback = match crontab_read(target, deadline).await {
        Ok(t) => t,
        Err(e) => return CronOpResult::refused(format!("installed but read-back failed: {e}")),
    };
    if readback == text {
        return CronOpResult::ok(String::new());
    }
    let mismatch = format!(
        "read back sha {} ({} bytes) != installed sha {} ({} bytes)",
        content_hash(&readback),
        readback.len(),
        content_hash(text),
        text.len()
    );
    match crontab_install(target, &before, deadline).await {
        Ok(()) => CronOpResult::refused(format!(
            "{mismatch}; reinstalled the previous crontab (sha {before_sha})"
        )),
        Err(e) => CronOpResult::refused(format!(
            "{mismatch}; ROLLBACK FAILED, crontab is in an unknown state: {e}"
        )),
    }
}

fn crontab_cmd(target: Option<&str>) -> Command {
    let mut cmd = Command::new("crontab");
    if let Some(user) = target {
        cmd.args(["-u", user]);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn crontab_read(
    target: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<String, String> {
    let limit = step_limit(deadline)?;
    let child = crontab_cmd(target)
        .arg("-l")
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn crontab: {e}"))?;
    let out = wait_bounded(child, limit)
        .await
        .map_err(|e| format!("crontab -l: {e}"))?;
    if out.status.success() {
        return String::from_utf8(out.stdout).map_err(|e| {
            format!(
                "crontab is not valid UTF-8 (byte {}); refusing to edit it",
                e.utf8_error().valid_up_to()
            )
        });
    }
    // cron implementations exit 1 with "no crontab for <user>" when none exists.
    if String::from_utf8_lossy(&out.stderr).contains("no crontab for") {
        return Ok(String::new());
    }
    Err(format!(
        "crontab -l exit {}: {}",
        out.status,
        stderr_snippet(&out.stderr)
    ))
}

async fn crontab_install(
    target: Option<&str>,
    text: &str,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let limit = step_limit(deadline)?;
    let mut child = crontab_cmd(target)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn crontab: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("write crontab stdin: {e}"))?;
        let _shut = stdin.shutdown().await;
    }
    let out = wait_bounded(child, limit)
        .await
        .map_err(|e| format!("crontab install: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "crontab install exit {}: {}",
            out.status,
            stderr_snippet(&out.stderr)
        ))
    }
}

async fn read_crontab(user: &str) -> anyhow::Result<String> {
    let r = run_cron(&CronOp::Read {
        user: user.to_string(),
    })
    .await;
    if !r.success {
        anyhow::bail!("read crontab for '{user}': {}", r.error);
    }
    Ok(r.text)
}

// ── parsing ──────────────────────────────────────────────────────────────────

/// One job line, enabled or commented out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CronEntry {
    /// 1-based line number in the crontab text.
    pub line_no: usize,
    /// Five time fields or an `@keyword`, single-space joined.
    pub schedule: String,
    pub command: String,
    /// False when the job line is commented out.
    pub enabled: bool,
    pub raw: String,
}

/// Splits `s` into its first `n` whitespace-delimited tokens and the remainder
/// (with its internal spacing preserved).
fn split_fields(s: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut fields = Vec::with_capacity(n);
    let mut rest = s.trim_start();
    for _ in 0..n {
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    Some((fields, rest))
}

fn is_time_field(f: &str, numeric_lead: bool) -> bool {
    let lead_ok = !numeric_lead || f.starts_with(|c: char| c.is_ascii_digit() || c == '*');
    lead_ok
        && f.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | ',' | '/' | '-'))
}

/// `(schedule, command)` for a job body, or `None` if it is not one. Minute,
/// hour and day-of-month must lead with a digit or `*`, so commented prose
/// (including the stock `# m h dom mon dow command` header) is not a job.
fn parse_job(body: &str) -> Option<(String, String)> {
    let body = body.trim();
    if body.starts_with('@') {
        let (fields, cmd) = split_fields(body, 1)?;
        let kw = fields[0];
        if !SCHEDULE_KEYWORDS.contains(&kw) || cmd.trim().is_empty() {
            return None;
        }
        return Some((kw.to_string(), cmd.trim_end().to_string()));
    }
    let (fields, cmd) = split_fields(body, 5)?;
    let fields_ok = fields
        .iter()
        .enumerate()
        .all(|(i, f)| is_time_field(f, i < 3));
    if !fields_ok || cmd.trim().is_empty() {
        return None;
    }
    Some((fields.join(" "), cmd.trim_end().to_string()))
}

fn is_env_line(line: &str) -> bool {
    line.split_once('=').is_some_and(|(k, _)| {
        let k = k.trim();
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

pub fn parse_entries(text: &str) -> Vec<CronEntry> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let trimmed = raw.trim_start();
            let (body, enabled) = match trimmed.strip_prefix('#') {
                Some(rest) => (rest, false),
                None => (trimmed, true),
            };
            if body.trim().is_empty() || (enabled && is_env_line(body)) {
                return None;
            }
            let (schedule, command) = parse_job(body)?;
            Some(CronEntry {
                line_no: i + 1,
                schedule,
                command,
                enabled,
                raw: raw.to_string(),
            })
        })
        .collect()
}

pub fn content_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── editing ──────────────────────────────────────────────────────────────────

#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum CronAction {
    /// Uncomment a job.
    Enable,
    /// Comment a job out.
    Disable,
    /// Append a job.
    Add,
    /// Delete a job line.
    Remove,
}

/// A fully-specified edit, built from the verb args by [`CronEditArgs::edit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronEdit {
    Enable(Selector),
    Disable {
        selector: Selector,
        marker: Option<String>,
    },
    Add {
        schedule: String,
        command: String,
    },
    Remove(Selector),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Line(usize),
    Command(String),
}

fn reject_newlines(label: &str, s: &str) -> anyhow::Result<()> {
    if s.contains(['\n', '\r', '\0']) {
        anyhow::bail!("{label} must be a single line with no NUL bytes");
    }
    Ok(())
}

/// cron turns an unescaped `%` into a newline and feeds the rest to stdin.
fn has_bare_percent(command: &str) -> bool {
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '%' => return true,
            _ => {}
        }
    }
    false
}

fn select(entries: &[CronEntry], sel: &Selector) -> anyhow::Result<CronEntry> {
    let found: Vec<&CronEntry> = match sel {
        Selector::Line(n) => entries.iter().filter(|e| e.line_no == *n).collect(),
        Selector::Command(c) => entries.iter().filter(|e| e.command == *c).collect(),
    };
    match found.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!("no cron job matches {sel:?}"),
        many => anyhow::bail!(
            "{} cron jobs match {sel:?} (lines {}); select by line instead",
            many.len(),
            many.iter()
                .map(|e| e.line_no.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Apply `edit` to `current`, returning the new crontab text. A no-op edit
/// (enabling an enabled job, adding an existing one) returns `current` as-is.
pub fn apply_edit(current: &str, edit: &CronEdit) -> anyhow::Result<String> {
    let entries = parse_entries(current);
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
    match edit {
        CronEdit::Enable(sel) => {
            let e = select(&entries, sel)?;
            if e.enabled {
                return Ok(current.to_string());
            }
            let body = e.raw.trim_start().trim_start_matches('#').trim_start();
            lines[e.line_no - 1] = body.to_string();
        }
        CronEdit::Disable { selector, marker } => {
            let e = select(&entries, selector)?;
            if e.enabled {
                lines[e.line_no - 1] = format!("#{}", e.raw);
                if let Some(m) = marker.as_deref().filter(|m| !m.trim().is_empty()) {
                    reject_newlines("marker", m)?;
                    lines.insert(e.line_no - 1, format!("# {}", m.trim()));
                }
            } else {
                return Ok(current.to_string());
            }
        }
        CronEdit::Add { schedule, command } => {
            reject_newlines("schedule", schedule)?;
            reject_newlines("command", command)?;
            if has_bare_percent(command) {
                anyhow::bail!(
                    "command has an unescaped `%` (cron reads it as a newline); write `\\%`"
                );
            }
            let line = format!("{} {}", schedule.trim(), command.trim());
            let (s, c) = parse_job(&line)
                .ok_or_else(|| anyhow::anyhow!("not a valid cron job line: {line}"))?;
            if entries
                .iter()
                .any(|e| e.enabled && e.schedule == s && e.command == c)
            {
                return Ok(current.to_string());
            }
            lines.push(line);
        }
        CronEdit::Remove(sel) => {
            let e = select(&entries, sel)?;
            lines.remove(e.line_no - 1);
        }
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    // cron ignores a final line without a trailing newline.
    Ok(lines.join("\n") + "\n")
}

/// Changed lines only, `-`/`+` prefixed, via LCS over lines.
pub fn line_diff(before: &str, after: &str) -> Vec<String> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(format!("-{}", a[i]));
            i += 1;
        } else {
            out.push(format!("+{}", b[j]));
            j += 1;
        }
    }
    out
}

// ── tools ────────────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemCronListArgs {
    /// Login name whose crontab to read.
    #[arg(long)]
    pub user: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SystemCronListOutput {
    pub user: String,
    /// Job lines, enabled and commented out. Env, comment and blank lines are
    /// only in `raw`.
    pub entries: Vec<CronEntry>,
    pub raw: String,
    /// sha256 of `raw`; pass as `expectedCurrent` to `system.cron.update`.
    pub hash: String,
}

/// Read a user's crontab on this host.
#[orca_tool(domain = "system.cron", verb = "list", role = "admin")]
async fn system_cron_list(
    args: SystemCronListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemCronListOutput> {
    validate_user(&args.user).map_err(anyhow::Error::msg)?;
    let raw = read_crontab(&args.user).await?;
    Ok(SystemCronListOutput {
        user: args.user,
        entries: parse_entries(&raw),
        hash: content_hash(&raw),
        raw,
    })
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CronEditArgs {
    /// Login name whose crontab to edit.
    #[arg(long)]
    pub user: String,
    /// Edit to make: `enable`, `disable`, `add` or `remove`.
    #[arg(long, value_enum)]
    pub action: Option<CronAction>,
    /// `enable`/`disable`/`remove`: select the job on this 1-based line.
    #[arg(long)]
    pub line: Option<usize>,
    /// `enable`/`disable`/`remove`: select the job whose command is exactly
    /// this. `add`: the command to run.
    #[arg(long)]
    pub command: Option<String>,
    /// `add`: five time fields or an `@keyword`.
    #[arg(long)]
    pub schedule: Option<String>,
    /// `disable`: comment line written above the disabled job.
    #[arg(long)]
    pub marker: Option<String>,
    /// `update`: hash from `system.cron.list`/`diff`; the edit is refused if
    /// the live crontab no longer matches it.
    #[arg(long)]
    pub expected_current: Option<String>,
}

impl CronEditArgs {
    fn edit(&self) -> anyhow::Result<CronEdit> {
        let action = self
            .action
            .ok_or_else(|| anyhow::anyhow!("`action` is required (enable|disable|add|remove)"))?;
        for (label, v) in [
            ("command", &self.command),
            ("schedule", &self.schedule),
            ("marker", &self.marker),
            ("expectedCurrent", &self.expected_current),
        ] {
            if let Some(v) = v {
                reject_newlines(label, v)?;
            }
        }
        let selector = || -> anyhow::Result<Selector> {
            match (self.line, self.command.as_deref()) {
                (Some(n), None) => Ok(Selector::Line(n)),
                (None, Some(c)) if !c.is_empty() => Ok(Selector::Command(c.to_string())),
                _ => anyhow::bail!("select the job with exactly one of `line` or `command`"),
            }
        };
        Ok(match action {
            CronAction::Enable => CronEdit::Enable(selector()?),
            CronAction::Disable => CronEdit::Disable {
                selector: selector()?,
                marker: self.marker.clone(),
            },
            CronAction::Remove => CronEdit::Remove(selector()?),
            CronAction::Add => CronEdit::Add {
                schedule: self
                    .schedule
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("`add` requires `schedule`"))?,
                command: self
                    .command
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("`add` requires `command`"))?,
            },
        })
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CronChangeOutput {
    pub user: String,
    /// False when the edit was a no-op.
    pub changed: bool,
    /// True only when the new crontab was installed and read back identical.
    pub applied: bool,
    pub before: String,
    pub after: String,
    /// Changed lines, `-` removed / `+` added.
    pub diff: Vec<String>,
    /// sha256 of `before`.
    pub expected_current: String,
}

fn plan(user: &str, before: String, edit: &CronEdit) -> anyhow::Result<CronChangeOutput> {
    let after = apply_edit(&before, edit)?;
    Ok(CronChangeOutput {
        user: user.to_string(),
        changed: after != before,
        applied: false,
        diff: line_diff(&before, &after),
        expected_current: content_hash(&before),
        before,
        after,
    })
}

fn required_expected(args: &CronEditArgs) -> anyhow::Result<&str> {
    args.expected_current
        .as_deref()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("`expectedCurrent` is required; get it from system.cron.diff")
        })
}

/// [`plan`], refused when `before` is not the crontab `expected` was taken from.
/// The helper repeats this check atomically with the write; this one fails
/// fast and keeps a stale plan from reaching it.
fn plan_against(
    user: &str,
    before: String,
    edit: &CronEdit,
    expected: &str,
) -> anyhow::Result<CronChangeOutput> {
    let out = plan(user, before, edit)?;
    if out.expected_current != expected {
        anyhow::bail!(
            "crontab for '{user}' changed since it was planned (expected {expected}, now {}); \
             re-run system.cron.diff",
            out.expected_current
        );
    }
    Ok(out)
}

/// Preview a crontab edit: the before/after text and diff, plus the
/// `expectedCurrent` hash to hand to `system.cron.update`. Changes nothing.
#[orca_tool(domain = "system.cron", verb = "diff", role = "admin")]
async fn system_cron_diff(
    args: CronEditArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<CronChangeOutput> {
    validate_user(&args.user).map_err(anyhow::Error::msg)?;
    let edit = args.edit()?;
    let before = read_crontab(&args.user).await?;
    plan(&args.user, before, &edit)
}

/// [MUTATES STATE] Edit a user's crontab: enable, disable, add or remove a job.
/// Requires `expectedCurrent` (from `system.cron.list`/`diff`) and refuses if
/// the crontab changed since. Installs via `crontab -`, reads it back, and
/// reinstalls the previous crontab if the read-back differs. Users other than
/// the daemon's own must be listed in `/etc/orca/cron-users`; root never is.
// Control-plane write: `data_mutation = false` keeps the `can_mutate` opt-in
// from reaching it, so only a real admin can rewrite a crontab.
#[orca_tool(
    domain = "system.cron",
    verb = "update",
    role = "admin",
    data_mutation = false
)]
async fn system_cron_update(
    args: CronEditArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<CronChangeOutput> {
    validate_user(&args.user).map_err(anyhow::Error::msg)?;
    let edit = args.edit()?;
    let expected = required_expected(&args)?;
    let before = read_crontab(&args.user).await?;
    let mut out = plan_against(&args.user, before, &edit, expected)?;
    if !out.changed {
        return Ok(out);
    }
    let w = run_cron(&CronOp::Write {
        user: args.user.clone(),
        text: out.after.clone(),
        expected_sha: expected.to_string(),
    })
    .await;
    if !w.success {
        anyhow::bail!("install crontab for '{}': {}", args.user, w.error);
    }
    out.applied = true;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# m h  dom mon dow   command
SHELL=/bin/bash
MAILTO=\"\"

*/5 * * * * /usr/bin/backup.sh --fast
#0 3 * * 1 /usr/local/bin/weekly
@reboot   /opt/start.sh  arg
# just a note about things
";

    #[test]
    fn parses_jobs_and_skips_comments_env_and_blanks() {
        let e = parse_entries(SAMPLE);
        assert_eq!(e.len(), 3, "{e:?}");
        assert_eq!(e[0].line_no, 5);
        assert_eq!(e[0].schedule, "*/5 * * * *");
        assert_eq!(e[0].command, "/usr/bin/backup.sh --fast");
        assert!(e[0].enabled);
        assert_eq!(e[1].line_no, 6);
        assert_eq!(e[1].schedule, "0 3 * * 1");
        assert!(!e[1].enabled);
        assert_eq!(e[2].schedule, "@reboot");
        assert_eq!(e[2].command, "/opt/start.sh  arg");
        assert_eq!(e[2].raw, "@reboot   /opt/start.sh  arg");
    }

    #[test]
    fn unknown_keyword_and_short_lines_are_not_jobs() {
        assert!(parse_entries("@sometimes foo\n1 2 3 cmd\n").is_empty());
    }

    #[test]
    fn disable_by_command_with_marker() {
        let edit = CronEdit::Disable {
            selector: Selector::Command("/usr/bin/backup.sh --fast".into()),
            marker: Some("orca: paused".into()),
        };
        let after = apply_edit(SAMPLE, &edit).unwrap();
        assert!(after.contains("# orca: paused\n#*/5 * * * * /usr/bin/backup.sh --fast\n"));
        let e = parse_entries(&after);
        assert!(
            !e.iter()
                .find(|e| e.schedule == "*/5 * * * *")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn enable_by_line_and_noop_when_already_enabled() {
        let after = apply_edit(SAMPLE, &CronEdit::Enable(Selector::Line(6))).unwrap();
        assert!(after.contains("\n0 3 * * 1 /usr/local/bin/weekly\n"));
        let same = apply_edit(SAMPLE, &CronEdit::Enable(Selector::Line(5))).unwrap();
        assert_eq!(same, SAMPLE);
    }

    #[test]
    fn add_appends_and_is_idempotent() {
        let edit = CronEdit::Add {
            schedule: "15 2 * * *".into(),
            command: "/bin/true".into(),
        };
        let after = apply_edit(SAMPLE, &edit).unwrap();
        assert!(after.ends_with("15 2 * * * /bin/true\n"));
        assert_eq!(apply_edit(&after, &edit).unwrap(), after);
    }

    #[test]
    fn add_rejects_invalid_schedule_and_newline_injection() {
        let bad = CronEdit::Add {
            schedule: "every day".into(),
            command: "/bin/true".into(),
        };
        assert!(apply_edit(SAMPLE, &bad).is_err());
        let inject = CronEdit::Add {
            schedule: "* * * * *".into(),
            command: "/bin/true\n* * * * * rm -rf /".into(),
        };
        assert!(apply_edit(SAMPLE, &inject).is_err());
    }

    #[test]
    fn remove_by_line_and_unmatched_selector_errors() {
        let after = apply_edit(SAMPLE, &CronEdit::Remove(Selector::Line(7))).unwrap();
        assert!(!after.contains("@reboot"));
        assert_eq!(after.lines().count(), SAMPLE.lines().count() - 1);
        assert!(apply_edit(SAMPLE, &CronEdit::Remove(Selector::Line(1))).is_err());
        assert!(apply_edit(SAMPLE, &CronEdit::Remove(Selector::Command("nope".into()))).is_err());
    }

    #[test]
    fn ambiguous_command_selector_is_refused() {
        let text = "* * * * * /x\n0 1 * * * /x\n";
        let err = apply_edit(text, &CronEdit::Remove(Selector::Command("/x".into())))
            .unwrap_err()
            .to_string();
        assert!(err.contains("lines 1, 2"), "{err}");
    }

    #[test]
    fn add_to_empty_crontab_gets_trailing_newline() {
        let edit = CronEdit::Add {
            schedule: "@daily".into(),
            command: "/bin/true".into(),
        };
        assert_eq!(apply_edit("", &edit).unwrap(), "@daily /bin/true\n");
    }

    #[test]
    fn diff_lists_only_changed_lines() {
        let after = apply_edit(SAMPLE, &CronEdit::Enable(Selector::Line(6))).unwrap();
        assert_eq!(
            line_diff(SAMPLE, &after),
            vec![
                "-#0 3 * * 1 /usr/local/bin/weekly".to_string(),
                "+0 3 * * 1 /usr/local/bin/weekly".to_string()
            ]
        );
    }

    #[test]
    fn plan_reports_hash_of_before_and_writes_nothing() {
        let out = plan(
            "orca",
            SAMPLE.to_string(),
            &CronEdit::Remove(Selector::Line(5)),
        )
        .unwrap();
        assert!(out.changed);
        assert!(!out.applied);
        assert_eq!(out.expected_current, content_hash(SAMPLE));
        assert_ne!(content_hash(&out.after), out.expected_current);
    }

    #[test]
    fn user_validation_rejects_injection() {
        for ok in ["orca", "_svc", "www-data", "a.b", "user1"] {
            assert!(validate_user(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-u",
            "root;id",
            "a b",
            "../etc",
            "Root",
            "x\n",
            "$(id)",
            "1abc",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_user(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn root_side_refuses_bad_user_without_running_crontab() {
        let r = execute_privileged_cron(CronOp::Write {
            user: "root;id".into(),
            text: String::new(),
            expected_sha: String::new(),
        })
        .await;
        assert!(!r.success);
        assert!(r.error.contains("invalid user"));
    }

    #[test]
    fn edit_args_require_exactly_one_selector() {
        let mut a = CronEditArgs {
            user: "orca".into(),
            action: Some(CronAction::Remove),
            ..Default::default()
        };
        assert!(a.edit().is_err());
        a.line = Some(3);
        a.command = Some("/x".into());
        assert!(a.edit().is_err());
        a.command = None;
        assert_eq!(a.edit().unwrap(), CronEdit::Remove(Selector::Line(3)));
    }

    #[test]
    fn update_requires_expected_current() {
        let mut a = CronEditArgs {
            user: "orca".into(),
            action: Some(CronAction::Remove),
            line: Some(1),
            ..Default::default()
        };
        assert!(required_expected(&a).is_err());
        a.expected_current = Some(String::new());
        assert!(required_expected(&a).is_err());
    }

    #[test]
    fn stale_plan_is_refused() {
        let edit = CronEdit::Remove(Selector::Line(5));
        let planned = content_hash(SAMPLE);
        let drifted = format!("{SAMPLE}@hourly /bin/new\n");
        let err = plan_against("orca", drifted, &edit, &planned)
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed since it was planned"), "{err}");
        assert!(plan_against("orca", SAMPLE.to_string(), &edit, &planned).is_ok());
    }

    #[test]
    fn allowlist_parses_logins_and_ignores_comments() {
        let a = parse_allowlist("# managed\nbackup\n\n  media  # arr stack\n");
        assert_eq!(a, vec!["backup".to_string(), "media".to_string()]);
    }

    #[test]
    fn allowlist_and_its_dirs_must_be_root_owned_and_not_group_or_world_writable() {
        let f = CRON_USERS_ALLOWLIST;
        assert!(check_root_only_writable(f, 0, 0o100644).is_ok());
        assert!(check_root_only_writable(f, 0, 0o100600).is_ok());
        assert!(check_root_only_writable(f, 1000, 0o100644).is_err());
        assert!(check_root_only_writable(f, 0, 0o100664).is_err());
        assert!(check_root_only_writable(f, 0, 0o100646).is_err());
        assert!(check_root_only_writable("/etc/orca", 0, 0o040755).is_ok());
        assert!(check_root_only_writable("/etc/orca", 0, 0o041777).is_err());
        assert!(check_root_only_writable("/etc", 999, 0o040755).is_err());
    }

    #[test]
    fn target_uid_requires_an_existing_non_root_user() {
        assert!(target_uid("ghost", None).is_err());
        assert!(target_uid("toor", Some(0)).unwrap_err().contains("uid 0"));
        assert_eq!(target_uid("orca", Some(999)), Ok(999));
    }

    #[cfg(unix)]
    #[test]
    fn root_and_unknown_users_are_refused_on_every_path() {
        assert!(target_uid("root", lookup_uid("root")).is_err());
        assert!(target_uid("no-such-user-x", lookup_uid("no-such-user-x")).is_err());
    }

    #[test]
    fn direct_path_is_refused_when_running_as_root() {
        assert!(direct_path_allowed(0).is_err());
        assert!(direct_path_allowed(1000).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_cron_refuses_root_before_choosing_a_path() {
        let r = run_cron(&CronOp::Read {
            user: "root".into(),
        })
        .await;
        assert!(!r.success);
        assert!(r.error.contains("uid 0"), "{}", r.error);
    }

    #[test]
    fn stderr_in_errors_is_capped() {
        let long = vec![b'e'; STDERR_SNIPPET_BYTES * 4];
        let s = stderr_snippet(&long);
        assert!(s.len() <= STDERR_SNIPPET_BYTES + " [truncated]".len());
        assert!(s.ends_with("[truncated]"));
        assert_eq!(stderr_snippet(b"  bad minute\n"), "bad minute");
    }

    #[tokio::test]
    async fn an_expired_deadline_stops_the_next_step() {
        let past = tokio::time::Instant::now() - Duration::from_secs(1);
        assert!(step_limit(past).is_err());
        let err = crontab_read(None, past).await.unwrap_err();
        assert!(err.contains("deadline"), "{err}");
        let soon = tokio::time::Instant::now() + Duration::from_secs(5);
        assert!(step_limit(soon).unwrap() <= Duration::from_secs(5));
    }

    #[test]
    fn root_is_refused_even_when_listed_and_unlisted_users_are_refused() {
        let list = vec!["root".to_string(), "backup".to_string(), "www".to_string()];
        assert!(authorize_target("root", 0, &list).is_err());
        assert!(authorize_target("toor", 0, &list).is_err());
        assert!(authorize_target("backup", 34, &list).is_ok());
        assert!(authorize_target("www", 1001, &list).is_ok());
        let err = authorize_target("daemon", 1, &list).unwrap_err();
        assert!(err.contains("system account"), "{err}");
        assert!(authorize_target("alice", 1000, &list).is_err());
    }

    #[test]
    fn add_rejects_bare_percent_but_allows_escaped() {
        let add = |c: &str| CronEdit::Add {
            schedule: "@daily".into(),
            command: c.into(),
        };
        assert!(apply_edit("", &add("date +%F")).is_err());
        assert!(apply_edit("", &add("date +\\%F")).is_ok());
        assert!(apply_edit("", &add("/bin/true")).is_ok());
    }

    #[test]
    fn nul_is_rejected_in_every_field() {
        let base = CronEditArgs {
            user: "orca".into(),
            action: Some(CronAction::Add),
            schedule: Some("@daily".into()),
            command: Some("/bin/true".into()),
            ..Default::default()
        };
        assert!(base.edit().is_ok());
        for field in ["command", "schedule", "marker", "expected"] {
            let mut a = CronEditArgs {
                user: base.user.clone(),
                action: base.action,
                schedule: base.schedule.clone(),
                command: base.command.clone(),
                ..Default::default()
            };
            let bad = Some("x\0y".to_string());
            match field {
                "command" => a.command = bad,
                "schedule" => a.schedule = bad,
                "marker" => a.marker = bad,
                _ => a.expected_current = bad,
            }
            assert!(a.edit().is_err(), "{field}");
        }
        assert!(validate_user("or\0ca").is_err());
        assert!(reject_bad_text("a\0b").is_err());
    }

    #[test]
    fn oversized_crontab_text_is_refused() {
        assert!(reject_bad_text(&"x".repeat(CRONTAB_MAX_BYTES + 1)).is_err());
        assert!(reject_bad_text("* * * * * /bin/true\n").is_ok());
    }

    #[tokio::test]
    async fn timeout_terminates_the_child() {
        let child = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let started = std::time::Instant::now();
        let err = wait_bounded(child, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
