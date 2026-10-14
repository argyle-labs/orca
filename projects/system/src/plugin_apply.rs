//! Root half of the generic plugin privileged seam, `orca admin plugin-apply`.
//!
//! A plugin that needs root on its own host (the unraid plugin writing
//! `/boot` templates, say) reaches it through `sudo -n <orca> admin
//! plugin-apply` with `{plugin, op, payload}` on stdin. This module, running as
//! root behind that `sudo`, never trusts the request:
//!
//! 1. `plugin` is a bare name resolved only inside the installed-plugin dir;
//!    anything path-shaped is refused before touching the filesystem.
//! 2. The binary's sha256 must equal the hash recorded in the root-owned
//!    [`TrustStore`] by `orca admin plugin-trust`, a root step the daemon's
//!    sudoers grant does not cover, so the caller can neither supply nor
//!    influence the expected hash. The binary, owned by the invoking user, is
//!    read through one `O_NOFOLLOW` fd and those verified bytes are staged in
//!    a sealed memfd (or a fixed root-only directory where memfd is
//!    unavailable) and run, so the file cannot change between check and exec.
//! 3. The staged copy runs as `<plugin> --privileged-op` with `{op, payload}`
//!    on stdin, a cleared environment, a fixed `PATH`, its own process group,
//!    and a timeout covering the whole apply. The
//!    plugin owns the op set and refuses anything outside it.
//!
//! The plugin's stdout and exit status are relayed unchanged; the plugin side
//! (`plugin_toolkit::privileged`) reads the last JSON line as `{ok, detail |
//! error}`.

// `payload` is the plugin's own op schema, opaque to core: it is forwarded to
// the plugin verbatim and never interpreted here.
#![allow(clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use plugin_toolkit::privileged::CALLER_UID_ENV;

/// Argument that puts a plugin binary into one-shot privileged mode.
pub const PRIVILEGED_FLAG: &str = "--privileged-op";

/// Long enough for the slowest known op (copying a docker volume into
/// appdata), short enough that a wedged plugin cannot hold root forever.
pub const PLUGIN_APPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The only `PATH` a privileged plugin sees.
pub const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Refuse to stage anything larger; plugin binaries are tens of MiB.
const MAX_PLUGIN_BYTES: u64 = 512 * 1024 * 1024;

/// A minisign signature is a few hundred bytes plus its trusted comment.
const MAX_SIGNATURE_BYTES: u64 = 4096;

/// One request on the seam's stdin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginApplyRequest {
    pub plugin: String,
    pub op: String,
    #[serde(default)]
    pub payload: Option<Value>,
}

/// What the plugin receives on stdin: the request minus `plugin`.
#[derive(Serialize)]
struct PluginOp<'a> {
    op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a Value>,
}

/// The reply shape the seam prints when it refuses before running a plugin.
/// Matches what plugins print, so a caller parses one shape either way.
#[derive(Debug, Serialize, Deserialize)]
pub struct SeamReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SeamReply {
    pub fn refused(error: &anyhow::Error) -> Self {
        Self {
            ok: false,
            detail: None,
            error: Some(format!("plugin-apply refused: {error:#}")),
        }
    }
}

/// The plugin's run, relayed verbatim.
#[derive(Debug)]
pub struct ApplyOutcome {
    /// The plugin's exit code; a signal death maps to 1.
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Parse the stdin request.
pub fn parse_request(input: &str) -> Result<PluginApplyRequest> {
    let req: PluginApplyRequest =
        serde_json::from_str(input).context("parse {plugin, op, payload} request")?;
    validate_plugin_name(&req.plugin)?;
    validate_op_name(&req.op)?;
    Ok(req)
}

/// A plugin name is a bare install-dir filename: lowercase alphanumerics, `-`
/// and `_`, starting alphanumeric. No separators, dots or traversal.
pub fn validate_plugin_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = name.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    ensure!(ok, "invalid plugin name {name:?}");
    Ok(())
}

fn validate_op_name(op: &str) -> Result<()> {
    let ok = !op.is_empty()
        && op.len() <= 64
        && op
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    ensure!(ok, "invalid op name {op:?}");
    Ok(())
}

/// Home directory of `uid` in passwd(5) text.
pub fn home_for_uid(passwd: &str, uid: u32) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 7 && f[2].parse::<u32>().ok() == Some(uid)).then(|| PathBuf::from(f[5]))
    })
}

/// The orca home of the user who invoked `sudo`: their passwd home plus the
/// state dir. `SUDO_UID` is set by sudo itself, and sudo strips a caller's
/// `ORCA_HOME`, so the passwd entry is the source of truth; a malformed
/// `SUDO_UID` or a uid with no passwd entry is an error. Outside sudo this is
/// the current process's own orca home.
pub fn invoking_orca_home() -> Result<PathBuf> {
    if std::env::var_os("SUDO_UID").is_some() {
        let uid = invoking_uid()?;
        let passwd = std::fs::read_to_string("/etc/passwd").context("read /etc/passwd")?;
        return home_for_uid(&passwd, uid)
            .map(|h| h.join(contract::config::APP_STATE_DIR))
            .with_context(|| format!("no passwd entry for SUDO_UID {uid}"));
    }
    files::ops::orca_home().context("no orca home for this process")
}

/// The invoking user's uid: `SUDO_UID` under sudo, else this process's own
/// euid (0 when root runs it directly). A present but non-numeric `SUDO_UID`
/// is refused rather than falling back.
pub fn invoking_uid() -> Result<u32> {
    match std::env::var("SUDO_UID") {
        Ok(raw) => raw
            .parse()
            .with_context(|| format!("SUDO_UID {raw:?} is not a uid")),
        // SAFETY: geteuid has no preconditions and cannot fail.
        Err(_) => Ok(unsafe { libc::geteuid() }),
    }
}

/// Root-owned directory of expected plugin hashes, one `<name>.sha256` each.
pub const PLUGIN_TRUST_DIR: &str = "/etc/orca/plugin-trust";

/// Fallback staging root when a sealed memfd is unavailable. Fixed, never
/// taken from the environment.
#[cfg(target_os = "linux")]
const STAGE_DIR: &str = "/run/orca/plugin-apply";
#[cfg(target_os = "linux")]
const STAGE_ANCHOR: &str = "/run";
// macOS `/var/run` is group-writable by `daemon`, so the checked chain stops
// at the orca-created dir below it.
#[cfg(not(target_os = "linux"))]
const STAGE_DIR: &str = "/var/run/orca/plugin-apply";
#[cfg(not(target_os = "linux"))]
const STAGE_ANCHOR: &str = "/var/run/orca";

/// A directory trusted only if it and every ancestor up to and including
/// `anchor`, and each file read from it, are owned by `owner_uid` and not
/// group/world-writable.
#[derive(Debug, Clone)]
pub struct TrustStore {
    pub dir: PathBuf,
    pub anchor: PathBuf,
    pub owner_uid: u32,
}

impl TrustStore {
    pub fn system() -> Self {
        Self {
            dir: PathBuf::from(PLUGIN_TRUST_DIR),
            anchor: PathBuf::from("/etc"),
            owner_uid: 0,
        }
    }

    fn staging() -> Self {
        Self {
            dir: PathBuf::from(STAGE_DIR),
            anchor: PathBuf::from(STAGE_ANCHOR),
            owner_uid: 0,
        }
    }

    #[cfg(test)]
    fn record_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.sha256"))
    }

    fn check_owner(&self, path: &Path, uid: u32, mode: u32) -> Result<()> {
        check_owner(path, uid, mode, self.owner_uid)
    }

    /// Checks the chain and returns `dir` with the anchor resolved. Only the
    /// anchor is canonicalized (macOS `/etc` is a symlink to `/private/etc`);
    /// every component below it must be a real directory.
    fn check_dirs(&self) -> Result<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        let rel = self.dir.strip_prefix(&self.anchor).with_context(|| {
            format!(
                "trust dir {} is outside {}",
                self.dir.display(),
                self.anchor.display()
            )
        })?;
        ensure!(
            rel.components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "trust dir {} is not a plain path",
            self.dir.display()
        );
        let anchor = std::fs::canonicalize(&self.anchor)
            .with_context(|| format!("resolve {}", self.anchor.display()))?;
        let real = anchor.join(rel);
        for dir in real.ancestors() {
            let meta = std::fs::symlink_metadata(dir)
                .with_context(|| format!("stat {}", dir.display()))?;
            ensure!(meta.is_dir(), "{} is not a directory", dir.display());
            self.check_owner(dir, meta.uid(), meta.mode())?;
            if dir == anchor {
                break;
            }
        }
        Ok(real)
    }

    /// The recorded sha256 for `name`, read through one fd whose owner and mode
    /// are checked.
    pub fn expected_hash(&self, name: &str) -> Result<String> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        validate_plugin_name(name)?;
        let untrusted = || {
            format!(
                "plugin '{name}' is not trusted on this host; an administrator must run \
                 `sudo orca admin plugin-trust {name}`"
            )
        };
        if !self.dir.exists() {
            bail!(untrusted());
        }
        let dir = self.check_dirs()?;
        let path = dir.join(format!("{name}.sha256"));
        let file = open_nofollow(&path, 0).with_context(untrusted)?;
        let meta = file.metadata().context("fstat trust record")?;
        ensure!(meta.is_file(), "{} is not a regular file", path.display());
        self.check_owner(&path, meta.uid(), meta.mode())?;
        let mut text = String::new();
        file.take(256)
            .read_to_string(&mut text)
            .with_context(|| format!("read {}", path.display()))?;
        let hex = text.trim().to_ascii_lowercase();
        ensure!(
            hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()),
            "{} is not a sha256",
            path.display()
        );
        Ok(hex)
    }

    /// Record `hex` as the expected hash for `name`, replacing any previous
    /// record atomically. Creates the store dir if missing.
    pub fn record(&self, name: &str, hex: &str) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        validate_plugin_name(name)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        let dir = self.check_dirs()?;
        let dest = dir.join(format!("{name}.sha256"));
        let tmp = dir.join(format!(".{name}.sha256.incoming"));
        _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(format!("{hex}\n").as_bytes())
            .and_then(|()| f.sync_all())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &dest).with_context(|| format!("install {}", dest.display()))?;
        Ok(())
    }
}

fn check_owner(path: &Path, uid: u32, mode: u32, owner_uid: u32) -> Result<()> {
    ensure!(
        uid == owner_uid,
        "{} is owned by uid {uid}, not {owner_uid}; refusing to trust it",
        path.display()
    );
    ensure!(
        mode & 0o022 == 0,
        "{} is group/world-writable (mode {:o}); refusing to trust it",
        path.display(),
        mode & 0o7777
    );
    Ok(())
}

fn check_private(path: &Path, uid: u32, mode: u32, owner_uid: u32, allowed: &[u32]) -> Result<()> {
    ensure!(
        uid == owner_uid && allowed.contains(&(mode & 0o7777)),
        "{} changed after staging (uid {uid}, mode {:o}); refusing to run it",
        path.display(),
        mode & 0o7777
    );
    Ok(())
}

fn open_nofollow(path: &Path, extra_flags: i32) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | extra_flags)
        .open(path)
}

/// The installed binary for `name` under `orca_home`, read through a single
/// `O_NOFOLLOW` fd so the bytes hashed are the bytes returned. The file must
/// be owned by `owner_uid` and not group/world-writable.
fn read_installed(orca_home: &Path, name: &str, owner_uid: u32) -> Result<Vec<u8>> {
    validate_plugin_name(name)?;
    let path = orca_home.join("plugins").join(name);
    read_owned(&path, name, owner_uid, MAX_PLUGIN_BYTES, || {
        format!("plugin '{name}' is not installed")
    })
}

/// The release signature saved beside the installed binary, read with the
/// same fd, owner and mode checks as the binary.
fn read_signature(orca_home: &Path, name: &str, owner_uid: u32) -> Result<String> {
    validate_plugin_name(name)?;
    let path = orca_home.join("plugins").join(format!("{name}.minisig"));
    let bytes = read_owned(&path, name, owner_uid, MAX_SIGNATURE_BYTES, || {
        format!(
            "plugin '{name}' has no release signature at {}",
            path.display()
        )
    })?;
    String::from_utf8(bytes).with_context(|| format!("{} is not text", path.display()))
}

/// Read `path` through one `O_NOFOLLOW` fd, owned by `owner_uid` and not
/// group/world-writable. `missing` describes a path that cannot be opened.
fn read_owned(
    path: &Path,
    name: &str,
    owner_uid: u32,
    max: u64,
    missing: impl FnOnce() -> String,
) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    // O_NONBLOCK so a FIFO planted at the path cannot block the open.
    let file = match open_nofollow(path, libc::O_NONBLOCK) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            bail!(
                "plugin '{name}' is not a regular file at {}",
                path.display()
            )
        }
        Err(e) => return Err(anyhow::Error::new(e).context(missing())),
    };
    let meta = file.metadata().context("fstat plugin file")?;
    ensure!(
        meta.is_file(),
        "plugin '{name}' is not a regular file at {}",
        path.display()
    );
    check_owner(path, meta.uid(), meta.mode(), owner_uid)?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open descriptor owned by `file`.
    let cleared = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) >= 0
    };
    ensure!(cleared, "clear O_NONBLOCK on {}", path.display());
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= max,
        "{} is implausibly large",
        path.display()
    );
    Ok(bytes)
}

/// Read the installed binary for `name`, owned by `owner_uid`, and check it
/// against the hash in `store`. Returns the verified bytes.
pub fn verified_binary(
    orca_home: &Path,
    store: &TrustStore,
    name: &str,
    owner_uid: u32,
) -> Result<Vec<u8>> {
    let expected = store.expected_hash(name)?;
    let bytes = read_installed(orca_home, name, owner_uid)?;
    ensure!(
        utils::hash::sha256_hex(&bytes) == expected,
        "plugin '{name}' binary does not match its trusted hash; an administrator must \
         re-run `sudo orca admin plugin-trust {name}` after reviewing the update"
    );
    Ok(bytes)
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// How [`trust`] decides a binary may be recorded.
#[derive(Debug, Clone, Copy)]
pub enum Signing<'a> {
    /// The `<name>.minisig` beside the binary must verify against one of these
    /// keys (see [`crate::plugin_sig`]).
    Required(&'a [&'a str]),
    /// Development override: record the binary without a signature.
    Unsigned,
}

/// What [`trust`] recorded.
#[derive(Debug)]
pub struct Trusted {
    pub sha256: String,
    /// The signed release version; `None` for an unsigned record.
    pub version: Option<String>,
}

/// Record the currently installed binary for `name`, owned by `owner_uid`, as
/// trusted once `signing` accepts it. Nothing is recorded on any failure. Only
/// a root caller can write the system store.
pub fn trust(
    orca_home: &Path,
    store: &TrustStore,
    name: &str,
    owner_uid: u32,
    signing: Signing<'_>,
) -> Result<Trusted> {
    let bytes = read_installed(orca_home, name, owner_uid)?;
    let trusted = match signing {
        Signing::Required(keys) => {
            let sig = read_signature(orca_home, name, owner_uid)?;
            let signed = crate::plugin_sig::verify(&bytes, &sig, name, keys)?;
            Trusted {
                sha256: signed.sha256,
                version: Some(signed.version),
            }
        }
        Signing::Unsigned => Trusted {
            sha256: utils::hash::sha256_hex(&bytes),
            version: None,
        },
    };
    store.record(name, &trusted.sha256)?;
    Ok(trusted)
}

/// Run one request: verify, stage, execute, relay. `caller_uid` is the
/// invoking user: it must own the installed binary and is passed to the
/// plugin as [`CALLER_UID_ENV`]. `timeout` bounds the whole run, including
/// reading the binary.
pub async fn apply(
    orca_home: &Path,
    store: &TrustStore,
    req: &PluginApplyRequest,
    caller_uid: u32,
    timeout: Duration,
) -> Result<ApplyOutcome> {
    apply_staged_in(
        orca_home,
        store,
        &TrustStore::staging(),
        req,
        caller_uid,
        timeout,
    )
    .await
}

async fn apply_staged_in(
    orca_home: &Path,
    store: &TrustStore,
    stage_dir: &TrustStore,
    req: &PluginApplyRequest,
    caller_uid: u32,
    timeout: Duration,
) -> Result<ApplyOutcome> {
    validate_plugin_name(&req.plugin)?;
    validate_op_name(&req.op)?;
    let input = serde_json::to_vec(&PluginOp {
        op: &req.op,
        payload: req.payload.as_ref(),
    })?;
    let (home, store, stage_dir, name) = (
        orca_home.to_path_buf(),
        store.clone(),
        stage_dir.clone(),
        req.plugin.clone(),
    );
    let run = async move {
        let stage = tokio::task::spawn_blocking(move || {
            let bytes = verified_binary(&home, &store, &name, caller_uid)?;
            Stage::new(&stage_dir, &name, &bytes)
        })
        .await
        .context("verify plugin")??;
        run_staged(&stage, &input, caller_uid).await
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(out) => out,
        Err(_) => bail!("plugin did not finish within {}s", timeout.as_secs_f32()),
    }
}

async fn run_staged(stage: &Stage, input: &[u8], caller_uid: u32) -> Result<ApplyOutcome> {
    use tokio::io::AsyncWriteExt;
    stage.verify()?;
    let bin = stage.exec_path();
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg(PRIVILEGED_FLAG)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .env(CALLER_UID_ENV, caller_uid.to_string())
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    if let Stage::Memfd(fd) = stage {
        use std::os::fd::AsRawFd;
        let fd = fd.as_raw_fd();
        // The memfd must survive exec: a `#!` plugin's interpreter reopens
        // `/proc/self/fd/N` after the exec. fcntl is async-signal-safe.
        // SAFETY: the closure only calls fcntl on an fd this process owns.
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let mut running = GroupKill {
        pgid: child.id().and_then(|p| i32::try_from(p).ok()),
        child,
    };
    let child = &mut running.child;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    if let Some(mut stdin) = stdin {
        // A plugin may exit without reading its op; its exit status is the
        // outcome, so a closed pipe is not an error.
        let written = match stdin.write_all(input).await {
            Ok(()) => stdin.shutdown().await,
            Err(e) => Err(e),
        };
        if let Err(e) = written
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(e).context("write op to plugin");
        }
    }
    let (status, stdout, stderr) =
        tokio::try_join!(child.wait(), read_all(stdout), read_all(stderr))
            .context("wait for plugin")?;
    Ok(ApplyOutcome {
        exit_code: status.code().unwrap_or(1),
        stdout,
        stderr,
    })
}

async fn read_all(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        p.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// Owns the plugin child and SIGKILLs its whole process group on drop, after
/// a normal exit too, so nothing it forked keeps running as root. `drop`
/// runs before the `child` field drops, so the group kill always comes first.
struct GroupKill {
    pgid: Option<i32>,
    child: tokio::process::Child,
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            // SAFETY: kill has no memory-safety preconditions.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

/// The verified bytes, staged where only root can change them.
enum Stage {
    /// A sealed, read-only memfd: no filesystem path an attacker can reach.
    #[cfg(target_os = "linux")]
    Memfd(std::os::fd::OwnedFd),
    /// A file in a fresh `mkdtemp` dir under the checked staging root,
    /// removed on drop.
    Dir {
        root: TrustStore,
        dir: PathBuf,
        path: PathBuf,
    },
}

impl Stage {
    fn new(root: &TrustStore, name: &str, bytes: &[u8]) -> Result<Self> {
        #[cfg(target_os = "linux")]
        if let Ok(fd) = sealed_memfd(bytes) {
            return Ok(Self::Memfd(fd));
        }
        Self::in_dir(root, name, bytes)
    }

    fn in_dir(root: &TrustStore, name: &str, bytes: &[u8]) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root.dir)
            .with_context(|| format!("create {}", root.dir.display()))?;
        let base = root.check_dirs()?;
        let template = std::ffi::CString::new(base.join("run-XXXXXX").into_os_string().into_vec())
            .context("staging path contains NUL")?;
        let raw = template.into_raw();
        // SAFETY: raw is a NUL-terminated buffer we own; mkdtemp rewrites the
        // X's in place and from_raw reclaims it.
        let made = unsafe { !libc::mkdtemp(raw).is_null() };
        let template = unsafe { std::ffi::CString::from_raw(raw) };
        ensure!(
            made,
            "mkdtemp in {}: {}",
            base.display(),
            std::io::Error::last_os_error()
        );
        let dir = PathBuf::from(std::ffi::OsString::from_vec(template.into_bytes()));
        let path = dir.join(name);
        let staged = Self::Dir {
            root: root.clone(),
            path: path.clone(),
            dir,
        };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("stage {}", path.display()))?;
        f.write_all(bytes).context("write staged plugin")?;
        f.sync_all().ok();
        Ok(staged)
    }

    /// Re-checks a directory stage right before exec: the staging chain, the
    /// per-run dir, and the staged file must still be owned by the root's
    /// owner with no group/world access.
    fn verify(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let (root, dir, path) = match self {
            #[cfg(target_os = "linux")]
            Self::Memfd(_) => return Ok(()),
            Self::Dir { root, dir, path } => (root, dir, path),
        };
        let base = root.check_dirs()?;
        ensure!(
            dir.parent() == Some(base.as_path()),
            "staging dir {} moved out of {}",
            dir.display(),
            base.display()
        );
        let meta =
            std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
        ensure!(meta.is_dir(), "{} is not a directory", dir.display());
        check_private(dir, meta.uid(), meta.mode(), root.owner_uid, &[0o700])?;
        let file = open_nofollow(path, libc::O_NONBLOCK)
            .with_context(|| format!("reopen staged {}", path.display()))?;
        let meta = file.metadata().context("fstat staged plugin")?;
        ensure!(meta.is_file(), "{} is not a regular file", path.display());
        check_private(
            path,
            meta.uid(),
            meta.mode(),
            root.owner_uid,
            &[0o700, 0o500],
        )
    }

    fn exec_path(&self) -> PathBuf {
        match self {
            #[cfg(target_os = "linux")]
            Self::Memfd(fd) => {
                use std::os::fd::AsRawFd;
                PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            }
            Self::Dir { path, .. } => path.clone(),
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        match self {
            #[cfg(target_os = "linux")]
            Self::Memfd(_) => {}
            Self::Dir { dir, .. } => _ = std::fs::remove_dir_all(dir),
        }
    }
}

/// `bytes` in a memfd sealed against any change, returned as a read-only fd.
/// The writable fd is closed before return: exec refuses a file with a
/// writer open (ETXTBSY).
#[cfg(target_os = "linux")]
fn sealed_memfd(bytes: &[u8]) -> Result<std::os::fd::OwnedFd> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    // SAFETY: the name is a valid C string; the result is checked.
    let raw = unsafe {
        libc::memfd_create(
            c"orca-plugin".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(
        raw >= 0,
        "memfd_create: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: raw is a fresh fd owned by nothing else.
    let mut f = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    f.write_all(bytes).context("write memfd")?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    // SAFETY: fcntl on an fd owned by `f`.
    let sealed = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, seals) } >= 0;
    ensure!(sealed, "seal memfd: {}", std::io::Error::last_os_error());
    let ro = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{raw}"))
        .context("reopen memfd read-only")?;
    drop(f);
    Ok(ro.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in plugin: answers `ping` with its environment, refuses unknown
    /// ops the way a real plugin's closed op set does, and `sleep`s on demand.
    const FAKE_PLUGIN: &str = r#"#!/bin/sh
[ "$1" = "--privileged-op" ] || { echo "not privileged mode" >&2; exit 9; }
read -r line
case "$line" in
  *'"op":"ping"'*) echo "$line" >&2; echo "{\"ok\":true,\"detail\":\"$PATH|${HOME:-unset}|$ORCA_CALLER_UID\"}" ;;
  *'"op":"sleep"'*) sleep 30 ;;
  *) echo '{"ok":false,"error":"unknown op"}'; exit 1 ;;
esac
"#;

    /// A trust store owned by the test user stands in for the root-owned one.
    fn store_in(home: &Path) -> TrustStore {
        TrustStore {
            dir: home.join("etc/orca/plugin-trust"),
            anchor: home.to_path_buf(),
            owner_uid: unsafe { libc::geteuid() },
        }
    }

    fn orca_home_with(name: &str, script: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join(name);
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        trust(
            home.path(),
            &store_in(home.path()),
            name,
            me(),
            Signing::Unsigned,
        )
        .unwrap();
        home
    }

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    /// A staging root owned by the test user stands in for the root-owned one.
    fn stage_in(home: &Path) -> TrustStore {
        TrustStore {
            dir: home.join("stage"),
            anchor: home.to_path_buf(),
            owner_uid: me(),
        }
    }

    async fn apply_in(home: &Path, r: &PluginApplyRequest, t: Duration) -> Result<ApplyOutcome> {
        apply_staged_in(home, &store_in(home), &stage_in(home), r, me(), t).await
    }

    fn req(plugin: &str, op: &str) -> PluginApplyRequest {
        PluginApplyRequest {
            plugin: plugin.into(),
            op: op.into(),
            payload: None,
        }
    }

    #[test]
    fn path_shaped_and_malformed_names_are_refused() {
        for bad in [
            "",
            "../unraid",
            "a/b",
            "/bin/sh",
            ".hidden",
            "Unraid",
            "un raid",
            "un.raid",
            "-x",
        ] {
            assert!(validate_plugin_name(bad).is_err(), "{bad:?}");
        }
        for good in ["unraid", "home-assistant", "pbs_2"] {
            validate_plugin_name(good).unwrap();
        }
        assert!(parse_request(r#"{"plugin":"../x","op":"ping"}"#).is_err());
        assert!(parse_request(r#"{"plugin":"unraid","op":"../x"}"#).is_err());
    }

    #[test]
    fn the_unraid_request_shape_parses_with_and_without_payload() {
        let r = parse_request(r#"{"plugin":"unraid","op":"ping"}"#).unwrap();
        assert_eq!((r.plugin.as_str(), r.op.as_str()), ("unraid", "ping"));
        assert!(r.payload.is_none());
        let r = parse_request(
            r#"{"op":"set_autostart","payload":{"name":"pbs","on":true},"plugin":"unraid"}"#,
        )
        .unwrap();
        assert_eq!(r.payload.unwrap()["name"], "pbs");
        assert!(parse_request(r#"{"plugin":"unraid","op":"ping","path":"/x"}"#).is_err());
    }

    #[test]
    fn the_safe_path_holds_only_root_owned_system_dirs() {
        assert_eq!(SAFE_PATH, "/usr/sbin:/usr/bin:/sbin:/bin");
    }

    #[test]
    fn a_staged_copy_altered_before_exec_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
        let home = tempfile::tempdir().unwrap();
        let staged = Stage::in_dir(&stage_in(home.path()), "fakeplug", b"x").unwrap();
        staged.verify().unwrap();
        let (root, dir, path) = match &staged {
            Stage::Dir { root, dir, path } => (root, dir, path),
            #[cfg(target_os = "linux")]
            Stage::Memfd(_) => unreachable!(),
        };
        set(path, 0o755).unwrap();
        assert!(staged.verify().is_err());
        set(path, 0o500).unwrap();
        staged.verify().unwrap();
        set(dir, 0o755).unwrap();
        assert!(staged.verify().is_err());
        set(dir, 0o700).unwrap();
        set(&root.dir, 0o777).unwrap();
        assert!(staged.verify().is_err());
        set(&root.dir, 0o700).unwrap();
        std::fs::remove_file(path).unwrap();
        std::os::unix::fs::symlink("/bin/sh", path).unwrap();
        assert!(staged.verify().is_err());
    }

    /// A killed child stays a zombie where nothing reaps orphans (containers
    /// without an init), and `kill(pid, 0)` still succeeds on it.
    fn alive(pid: i32) -> bool {
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
            .unwrap_or(false);
        !zombie && unsafe { libc::kill(pid, 0) } == 0
    }

    #[tokio::test]
    async fn a_normal_exit_kills_the_plugins_leftover_process_group() {
        let home = tempfile::tempdir().unwrap();
        let pidfile = home.path().join("child.pid");
        let script = format!(
            "#!/bin/sh\nsleep 60 >/dev/null 2>&1 &\necho $! > {}\nexit 0\n",
            pidfile.display()
        );
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = home.path().join("plugins");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("leaver"), &script).unwrap();
            std::fs::set_permissions(dir.join("leaver"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            trust(
                home.path(),
                &store_in(home.path()),
                "leaver",
                me(),
                Signing::Unsigned,
            )
            .unwrap();
        }
        let out = apply_in(home.path(), &req("leaver", "ping"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "background child {pid} survived"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_plugin_exiting_without_reading_its_op_reports_its_exit() {
        let home = orca_home_with("deaf", "#!/bin/sh\nexit 3\n");
        let mut r = req("deaf", "ping");
        r.payload = Some(serde_json::Value::String("x".repeat(1 << 20)));
        let out = apply_in(home.path(), &r, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, 3);
    }

    #[test]
    fn home_for_uid_reads_the_passwd_home() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      orca:x:999:999::/mnt/user/appdata/orca:/bin/bash\n";
        assert_eq!(
            home_for_uid(passwd, 999),
            Some(PathBuf::from("/mnt/user/appdata/orca"))
        );
        assert_eq!(home_for_uid(passwd, 5), None);
    }

    #[tokio::test]
    async fn a_verified_plugin_runs_with_a_clean_env_and_only_op_and_payload() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let out = apply_in(
            home.path(),
            &req("fakeplug", "ping"),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
        let reply: SeamReply = serde_json::from_slice(&out.stdout).unwrap();
        assert!(reply.ok);
        assert_eq!(reply.detail.unwrap(), format!("{SAFE_PATH}|unset|{}", me()));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            r#"{"op":"ping"}"#
        );
    }

    #[tokio::test]
    async fn an_unknown_op_is_refused_by_the_plugin_and_relayed() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let out = apply_in(
            home.path(),
            &req("fakeplug", "format_disk"),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 1);
        let reply: SeamReply = serde_json::from_slice(&out.stdout).unwrap();
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("unknown op"));
    }

    #[tokio::test]
    async fn an_unknown_plugin_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let err = apply_in(home.path(), &req("nosuch", "ping"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not trusted"), "{err:#}");
    }

    #[tokio::test]
    async fn a_tampered_binary_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let bin = home.path().join("plugins/fakeplug");
        std::fs::write(&bin, format!("{FAKE_PLUGIN}\necho pwned\n")).unwrap();
        let err = apply_in(
            home.path(),
            &req("fakeplug", "ping"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("does not match its trusted hash"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_binary_with_no_trust_record_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        std::fs::remove_file(store_in(home.path()).record_path("fakeplug")).unwrap();
        let err = apply_in(
            home.path(),
            &req("fakeplug", "ping"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not trusted"), "{err:#}");
    }

    #[tokio::test]
    async fn a_symlinked_plugin_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let dir = home.path().join("plugins");
        std::os::unix::fs::symlink(dir.join("fakeplug"), dir.join("linked")).unwrap();
        let store = store_in(home.path());
        std::fs::copy(store.record_path("fakeplug"), store.record_path("linked")).unwrap();
        let err = apply_in(home.path(), &req("linked", "ping"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err:#}");
    }

    #[tokio::test]
    async fn a_plugin_that_overruns_the_timeout_is_killed_and_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let started = std::time::Instant::now();
        let err = apply_in(
            home.path(),
            &req("fakeplug", "sleep"),
            Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_hash_sidecar_beside_the_binary_is_not_trusted() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("evil"), FAKE_PLUGIN).unwrap();
        let hex = utils::hash::sha256_hex(FAKE_PLUGIN.as_bytes());
        std::fs::write(dir.join(".evil.sha256"), format!("{hex}\n")).unwrap();
        let err = apply_in(home.path(), &req("evil", "ping"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not trusted"), "{err:#}");
    }

    #[test]
    fn a_store_owned_by_someone_else_is_not_trusted() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = TrustStore {
            owner_uid: store_in(home.path()).owner_uid.wrapping_add(1),
            ..store_in(home.path())
        };
        let err = verified_binary(home.path(), &store, "fakeplug", me()).unwrap_err();
        assert!(err.to_string().contains("refusing to trust"), "{err:#}");
    }

    #[test]
    fn a_writable_store_dir_or_record_is_not_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = store_in(home.path());
        for target in [
            store.dir.clone(),
            store.dir.parent().unwrap().to_path_buf(),
            home.path().to_path_buf(),
        ] {
            let mode = if target == home.path() { 0o700 } else { 0o755 };
            set(&target, 0o777).unwrap();
            let err = verified_binary(home.path(), &store, "fakeplug", me()).unwrap_err();
            assert!(err.to_string().contains("group/world-writable"), "{err:#}");
            set(&target, mode).unwrap();
        }
        let record = store.record_path("fakeplug");
        set(&record, 0o666).unwrap();
        let err = verified_binary(home.path(), &store, "fakeplug", me()).unwrap_err();
        assert!(err.to_string().contains("group/world-writable"), "{err:#}");
        set(&record, 0o644).unwrap();
        verified_binary(home.path(), &store, "fakeplug", me()).unwrap();
    }

    #[test]
    fn a_symlinked_trust_record_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = store_in(home.path());
        let record = store.record_path("fakeplug");
        let elsewhere = home.path().join("planted.sha256");
        std::fs::rename(&record, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &record).unwrap();
        assert!(verified_binary(home.path(), &store, "fakeplug", me()).is_err());
    }

    fn sig_fixture(file: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/plugin_sig/{file}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// An orca home with the signed `demo` fixture installed, its signature
    /// from `sig` (none when `None`), and the fixture key to verify against.
    fn signed_home(sig: Option<&str>) -> (tempfile::TempDir, String) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo"), sig_fixture("demo")).unwrap();
        if let Some(sig) = sig {
            std::fs::write(dir.join("demo.minisig"), sig_fixture(sig)).unwrap();
        }
        let key = sig_fixture("test.pub").lines().nth(1).unwrap().to_string();
        (home, key)
    }

    #[test]
    fn a_signed_binary_is_trusted_with_its_release_version() {
        let (home, key) = signed_home(Some("demo.minisig"));
        let store = store_in(home.path());
        let got = trust(
            home.path(),
            &store,
            "demo",
            me(),
            Signing::Required(&[&key]),
        )
        .unwrap();
        assert_eq!(got.version.as_deref(), Some("1.2.3"));
        assert_eq!(store.expected_hash("demo").unwrap(), got.sha256);
    }

    #[test]
    fn a_failed_signature_check_records_nothing() {
        for sig in [
            None,
            Some("demo.wrong-sha.minisig"),
            Some("demo.no-sha.minisig"),
        ] {
            let (home, key) = signed_home(sig);
            let store = store_in(home.path());
            let err = trust(
                home.path(),
                &store,
                "demo",
                me(),
                Signing::Required(&[&key]),
            );
            assert!(err.is_err(), "{sig:?} was trusted");
            assert!(!store.record_path("demo").exists(), "{sig:?} left a record");
        }
        let (home, _) = signed_home(Some("demo.minisig"));
        let other = sig_fixture("other.pub").lines().nth(1).unwrap().to_string();
        let store = store_in(home.path());
        let err = trust(
            home.path(),
            &store,
            "demo",
            me(),
            Signing::Required(&[&other]),
        );
        assert!(err.is_err());
        assert!(!store.record_path("demo").exists());
    }

    #[test]
    fn a_missing_signature_is_named_in_the_error() {
        let (home, key) = signed_home(None);
        let err = trust(
            home.path(),
            &store_in(home.path()),
            "demo",
            me(),
            Signing::Required(&[&key]),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no release signature"),
            "{err:#}"
        );
    }

    #[test]
    fn a_symlinked_signature_is_refused() {
        let (home, key) = signed_home(None);
        let real = home.path().join("elsewhere.minisig");
        std::fs::write(&real, sig_fixture("demo.minisig")).unwrap();
        std::os::unix::fs::symlink(&real, home.path().join("plugins/demo.minisig")).unwrap();
        let store = store_in(home.path());
        assert!(
            trust(
                home.path(),
                &store,
                "demo",
                me(),
                Signing::Required(&[&key])
            )
            .is_err()
        );
        assert!(!store.record_path("demo").exists());
    }

    #[test]
    fn a_retrust_replaces_the_record_and_new_files_are_0644() {
        use std::os::unix::fs::PermissionsExt;
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = store_in(home.path());
        let bin = home.path().join("plugins/fakeplug");
        std::fs::write(&bin, format!("{FAKE_PLUGIN}\n# v2\n")).unwrap();
        assert!(verified_binary(home.path(), &store, "fakeplug", me()).is_err());
        trust(home.path(), &store, "fakeplug", me(), Signing::Unsigned).unwrap();
        verified_binary(home.path(), &store, "fakeplug", me()).unwrap();
        let mode = std::fs::metadata(store.record_path("fakeplug"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn the_staged_copy_is_removed_after_use() {
        let home = tempfile::tempdir().unwrap();
        let root = stage_in(home.path());
        let staged = Stage::in_dir(&root, "fakeplug", b"x").unwrap();
        let (dir, path) = match &staged {
            Stage::Dir { dir, path, .. } => (dir.clone(), path.clone()),
            #[cfg(target_os = "linux")]
            Stage::Memfd(_) => unreachable!(),
        };
        assert!(path.is_file());
        assert!(dir.starts_with(std::fs::canonicalize(home.path()).unwrap()));
        drop(staged);
        assert!(!dir.exists());
    }

    #[test]
    fn staging_never_uses_the_temp_dir() {
        let tmp = std::env::temp_dir();
        assert!(!Path::new(STAGE_DIR).starts_with(&tmp));
        assert!(Path::new(STAGE_DIR).starts_with(STAGE_ANCHOR));
        #[cfg(target_os = "linux")]
        {
            let home = tempfile::tempdir().unwrap();
            let staged = Stage::new(&stage_in(home.path()), "fakeplug", b"x").unwrap();
            assert!(matches!(staged, Stage::Memfd(_)));
            assert!(staged.exec_path().starts_with("/proc/self/fd"));
        }
    }

    #[test]
    fn a_fifo_plugin_is_refused_without_blocking() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let fifo = home.path().join("plugins/fifoplug");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let store = store_in(home.path());
        store.record("fifoplug", &"0".repeat(64)).unwrap();
        let err = verified_binary(home.path(), &store, "fifoplug", me()).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err:#}");
    }

    #[test]
    fn a_binary_owned_by_another_user_or_group_writable_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = store_in(home.path());
        let err =
            verified_binary(home.path(), &store, "fakeplug", me().wrapping_add(1)).unwrap_err();
        assert!(err.to_string().contains("refusing to trust"), "{err:#}");
        let bin = home.path().join("plugins/fakeplug");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o775)).unwrap();
        let err = verified_binary(home.path(), &store, "fakeplug", me()).unwrap_err();
        assert!(err.to_string().contains("group/world-writable"), "{err:#}");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_plugins_whole_process_group() {
        let home = tempfile::tempdir().unwrap();
        let pidfile = home.path().join("child.pid");
        let script = format!(
            // Rename so the pidfile never appears empty if the kill lands mid-write.
            "#!/bin/sh\nsleep 60 &\necho $! > {0}.tmp\nmv {0}.tmp {0}\nwait\n",
            pidfile.display()
        );
        let home = {
            use std::os::unix::fs::PermissionsExt;
            let dir = home.path().join("plugins");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("forker"), &script).unwrap();
            std::fs::set_permissions(dir.join("forker"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            trust(
                home.path(),
                &store_in(home.path()),
                "forker",
                me(),
                Signing::Unsigned,
            )
            .unwrap();
            home
        };
        // Under load the timeout can fire before the script forks and writes the
        // pidfile; rerun with a longer budget until the child exists to be checked.
        for secs in [2, 4, 8, 16] {
            let err = apply_in(
                home.path(),
                &req("forker", "ping"),
                Duration::from_secs(secs),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("did not finish"), "{err:#}");
            if pidfile.exists() {
                break;
            }
        }
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pidfile never written within the 16s budget")
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "background child {pid} survived"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
