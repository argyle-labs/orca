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
//!    influence the expected hash. The binary is read through one
//!    `O_NOFOLLOW` fd and those verified bytes are staged into a root-only
//!    directory and run, so the file cannot change between check and exec.
//! 3. The staged copy runs as `<plugin> --privileged-op` with `{op, payload}`
//!    on stdin, a cleared environment, a fixed `PATH`, and a timeout. The
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

/// Argument that puts a plugin binary into one-shot privileged mode.
pub const PRIVILEGED_FLAG: &str = "--privileged-op";

/// Long enough for the slowest known op (copying a docker volume into
/// appdata), short enough that a wedged plugin cannot hold root forever.
pub const PLUGIN_APPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The only `PATH` a privileged plugin sees.
pub const SAFE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Refuse to stage anything larger; plugin binaries are tens of MiB.
const MAX_PLUGIN_BYTES: u64 = 512 * 1024 * 1024;

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
/// `ORCA_HOME`, so the passwd entry is the source of truth. Outside sudo this
/// is the current process's own orca home.
pub fn invoking_orca_home() -> Option<PathBuf> {
    if let Some(uid) = std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok()) {
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        return home_for_uid(&passwd, uid).map(|h| h.join(contract::config::APP_STATE_DIR));
    }
    files::ops::orca_home()
}

/// Root-owned directory of expected plugin hashes, one `<name>.sha256` each.
pub const PLUGIN_TRUST_DIR: &str = "/etc/orca/plugin-trust";

/// Where expected plugin hashes live. Every directory from `dir` up to and
/// including `anchor`, and each record, must be owned by `owner_uid` and not
/// group/world-writable, or nothing in it is trusted.
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

    fn record_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.sha256"))
    }

    fn check_owner(&self, path: &Path, uid: u32, mode: u32) -> Result<()> {
        ensure!(
            uid == self.owner_uid,
            "{} is owned by uid {uid}, not {}; refusing to trust it",
            path.display(),
            self.owner_uid
        );
        ensure!(
            mode & 0o022 == 0,
            "{} is group/world-writable (mode {:o}); refusing to trust it",
            path.display(),
            mode & 0o7777
        );
        Ok(())
    }

    fn check_dirs(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            self.dir.starts_with(&self.anchor),
            "trust dir {} is outside {}",
            self.dir.display(),
            self.anchor.display()
        );
        for dir in self.dir.ancestors() {
            let meta = std::fs::symlink_metadata(dir)
                .with_context(|| format!("stat {}", dir.display()))?;
            ensure!(meta.is_dir(), "{} is not a directory", dir.display());
            self.check_owner(dir, meta.uid(), meta.mode())?;
            if dir == self.anchor {
                break;
            }
        }
        Ok(())
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
        self.check_dirs()?;
        let path = self.record_path(name);
        let file = open_nofollow(&path).with_context(untrusted)?;
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
        self.check_dirs()?;
        let dest = self.record_path(name);
        let tmp = self.dir.join(format!(".{name}.sha256.incoming"));
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

fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// The installed binary for `name` under `orca_home`, read through a single
/// `O_NOFOLLOW` fd so the bytes hashed are the bytes returned.
fn read_installed(orca_home: &Path, name: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    validate_plugin_name(name)?;
    let path = orca_home.join("plugins").join(name);
    let file = match open_nofollow(&path) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            bail!(
                "plugin '{name}' is not a regular file at {}",
                path.display()
            )
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("plugin '{name}' is not installed")));
        }
    };
    let meta = file.metadata().context("fstat plugin binary")?;
    ensure!(
        meta.is_file(),
        "plugin '{name}' is not a regular file at {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(MAX_PLUGIN_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_PLUGIN_BYTES,
        "plugin '{name}' is implausibly large"
    );
    Ok(bytes)
}

/// Read the installed binary for `name` and check it against the hash in
/// `store`. Returns the verified bytes.
pub fn verified_binary(orca_home: &Path, store: &TrustStore, name: &str) -> Result<Vec<u8>> {
    let expected = store.expected_hash(name)?;
    let bytes = read_installed(orca_home, name)?;
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

/// Record the currently installed binary for `name` as trusted. Only a root
/// caller can write the system store.
pub fn trust(orca_home: &Path, store: &TrustStore, name: &str) -> Result<String> {
    let hex = utils::hash::sha256_hex(&read_installed(orca_home, name)?);
    store.record(name, &hex)?;
    Ok(hex)
}

/// Run one request: verify, stage, execute, relay.
pub async fn apply(
    orca_home: &Path,
    store: &TrustStore,
    req: &PluginApplyRequest,
    timeout: Duration,
) -> Result<ApplyOutcome> {
    validate_plugin_name(&req.plugin)?;
    validate_op_name(&req.op)?;
    let bytes = verified_binary(orca_home, store, &req.plugin)?;
    let stage = StagedBinary::new(&req.plugin, &bytes)?;
    let input = serde_json::to_vec(&PluginOp {
        op: &req.op,
        payload: req.payload.as_ref(),
    })?;
    run_staged(&stage.path, &input, timeout).await
}

async fn run_staged(bin: &Path, input: &[u8], timeout: Duration) -> Result<ApplyOutcome> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(bin)
        .arg(PRIVILEGED_FLAG)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input).await.context("write op to plugin")?;
        stdin.shutdown().await.context("close plugin stdin")?;
    }
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => out.context("wait for plugin")?,
        Err(_) => bail!("plugin did not finish within {}s", timeout.as_secs_f32()),
    };
    Ok(ApplyOutcome {
        exit_code: out.status.code().unwrap_or(1),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// The verified bytes, written into a fresh root-only directory and removed on
/// drop. Fresh `create` (never reuse) means a pre-planted path fails instead
/// of being followed.
struct StagedBinary {
    dir: PathBuf,
    path: PathBuf,
}

impl StagedBinary {
    fn new(name: &str, bytes: &[u8]) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("orca-plugin-apply-{}-{nanos}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let staged = Self {
            path: dir.join(name),
            dir,
        };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&staged.path)
            .with_context(|| format!("stage {}", staged.path.display()))?;
        f.write_all(bytes).context("write staged plugin")?;
        f.sync_all().ok();
        Ok(staged)
    }
}

impl Drop for StagedBinary {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.dir);
    }
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
  *'"op":"ping"'*) echo "$line" >&2; echo "{\"ok\":true,\"detail\":\"$PATH|${HOME:-unset}\"}" ;;
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
        trust(home.path(), &store_in(home.path()), name).unwrap();
        home
    }

    async fn apply_in(home: &Path, r: &PluginApplyRequest, t: Duration) -> Result<ApplyOutcome> {
        apply(home, &store_in(home), r, t).await
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
        assert_eq!(reply.detail.unwrap(), format!("{SAFE_PATH}|unset"));
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
        let err = verified_binary(home.path(), &store, "fakeplug").unwrap_err();
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
            let err = verified_binary(home.path(), &store, "fakeplug").unwrap_err();
            assert!(err.to_string().contains("group/world-writable"), "{err:#}");
            set(&target, mode).unwrap();
        }
        let record = store.record_path("fakeplug");
        set(&record, 0o666).unwrap();
        let err = verified_binary(home.path(), &store, "fakeplug").unwrap_err();
        assert!(err.to_string().contains("group/world-writable"), "{err:#}");
        set(&record, 0o644).unwrap();
        verified_binary(home.path(), &store, "fakeplug").unwrap();
    }

    #[test]
    fn a_symlinked_trust_record_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = store_in(home.path());
        let record = store.record_path("fakeplug");
        let elsewhere = home.path().join("planted.sha256");
        std::fs::rename(&record, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &record).unwrap();
        assert!(verified_binary(home.path(), &store, "fakeplug").is_err());
    }

    #[test]
    fn a_retrust_replaces_the_record_and_new_files_are_0644() {
        use std::os::unix::fs::PermissionsExt;
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let store = store_in(home.path());
        let bin = home.path().join("plugins/fakeplug");
        std::fs::write(&bin, format!("{FAKE_PLUGIN}\n# v2\n")).unwrap();
        assert!(verified_binary(home.path(), &store, "fakeplug").is_err());
        trust(home.path(), &store, "fakeplug").unwrap();
        verified_binary(home.path(), &store, "fakeplug").unwrap();
        let mode = std::fs::metadata(store.record_path("fakeplug"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn the_staged_copy_is_removed_after_use() {
        let staged = StagedBinary::new("fakeplug", b"x").unwrap();
        let dir = staged.dir.clone();
        assert!(staged.path.is_file());
        drop(staged);
        assert!(!dir.exists());
    }
}
