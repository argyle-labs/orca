//! Root half of the generic plugin privileged seam, `orca admin plugin-apply`.
//!
//! A plugin that needs root on its own host (the unraid plugin writing
//! `/boot` templates, say) reaches it through `sudo -n <orca> admin
//! plugin-apply` with `{plugin, op, payload}` on stdin. This module, running as
//! root behind that `sudo`, never trusts the request:
//!
//! 1. `plugin` is a bare name resolved only inside the installed-plugin dir;
//!    anything path-shaped is refused before touching the filesystem.
//! 2. The binary's sha256 must equal the hash the installer recorded beside
//!    it, and the verified bytes are what runs: they are staged into a
//!    root-only directory first, so the file cannot change between check and
//!    exec.
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

/// Where the installer records a plugin binary's sha256.
pub fn hash_sidecar(plugin_dir: &Path, name: &str) -> PathBuf {
    plugin_dir.join(format!(".{name}.sha256"))
}

/// Record the sha256 of the installed binary `plugin_dir/name`, replacing any
/// previous record atomically.
pub fn record_install_hash(plugin_dir: &Path, name: &str) -> Result<String> {
    validate_plugin_name(name)?;
    let hex = utils::hash::sha256_file(&plugin_dir.join(name))?;
    let dest = hash_sidecar(plugin_dir, name);
    let tmp = plugin_dir.join(format!(".{name}.sha256.incoming"));
    std::fs::write(&tmp, format!("{hex}\n")).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &dest).with_context(|| format!("install {}", dest.display()))?;
    Ok(hex)
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

/// Read the installed binary for `name` under `orca_home` and check it against
/// the recorded hash. Returns the verified bytes.
pub fn verified_binary(orca_home: &Path, name: &str) -> Result<Vec<u8>> {
    validate_plugin_name(name)?;
    let dir = orca_home.join("plugins");
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path)
        .with_context(|| format!("plugin '{name}' is not installed"))?;
    ensure!(
        meta.file_type().is_file(),
        "plugin '{name}' is not a regular file at {}",
        path.display()
    );
    ensure!(
        meta.len() <= MAX_PLUGIN_BYTES,
        "plugin '{name}' is implausibly large"
    );
    let recorded = std::fs::read_to_string(hash_sidecar(&dir, name)).with_context(|| {
        format!(
            "plugin '{name}' has no recorded install hash; reinstall it so the installer \
             records one"
        )
    })?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let actual = utils::hash::sha256_hex(&bytes);
    ensure!(
        actual == recorded.trim().to_ascii_lowercase(),
        "plugin '{name}' binary does not match its recorded install hash"
    );
    Ok(bytes)
}

/// Run one request: verify, stage, execute, relay.
pub async fn apply(
    orca_home: &Path,
    req: &PluginApplyRequest,
    timeout: Duration,
) -> Result<ApplyOutcome> {
    validate_plugin_name(&req.plugin)?;
    validate_op_name(&req.op)?;
    let bytes = verified_binary(orca_home, &req.plugin)?;
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

    fn orca_home_with(name: &str, script: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join(name);
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        record_install_hash(&dir, name).unwrap();
        home
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
        let out = apply(
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
        let out = apply(
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
        let err = apply(home.path(), &req("nosuch", "ping"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err:#}");
    }

    #[tokio::test]
    async fn a_tampered_binary_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let bin = home.path().join("plugins/fakeplug");
        std::fs::write(&bin, format!("{FAKE_PLUGIN}\necho pwned\n")).unwrap();
        let err = apply(
            home.path(),
            &req("fakeplug", "ping"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("recorded install hash"), "{err:#}");
    }

    #[tokio::test]
    async fn a_binary_with_no_recorded_hash_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        std::fs::remove_file(hash_sidecar(&home.path().join("plugins"), "fakeplug")).unwrap();
        let err = apply(
            home.path(),
            &req("fakeplug", "ping"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no recorded install hash"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_symlinked_plugin_is_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let dir = home.path().join("plugins");
        std::os::unix::fs::symlink(dir.join("fakeplug"), dir.join("linked")).unwrap();
        std::fs::copy(hash_sidecar(&dir, "fakeplug"), hash_sidecar(&dir, "linked")).unwrap();
        let err = apply(home.path(), &req("linked", "ping"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err:#}");
    }

    #[tokio::test]
    async fn a_plugin_that_overruns_the_timeout_is_killed_and_refused() {
        let home = orca_home_with("fakeplug", FAKE_PLUGIN);
        let started = std::time::Instant::now();
        let err = apply(
            home.path(),
            &req("fakeplug", "sleep"),
            Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
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
