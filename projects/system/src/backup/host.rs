//! The `host` backup provider — orca's own host configuration.
//!
//! "Host config" is declarative and config-driven ([[orca-must-be-declarative-config-driven]]):
//! the include/exclude path set comes from a `backup`/`host` config row so a
//! deployment decides what its host backup captures (e.g. the mac + hemlock add
//! the Claude memory dir). With no row, a conservative default captures orca's
//! own config under the state dir — orca.toml, PKI, profiles, memory — but NOT
//! the DB or logs, which are local history, not portable config
//! ([[data-classification-config-syncs-history-local]]).
//!
//! Payload layout is a reversible mirror: an absolute source path `/a/b/c` is
//! copied to `<payload>/a/b/c`, so restore is "walk the payload, copy each file
//! back to `/` + its relative path". No manifest of sources needed — the tree
//! IS the manifest.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use contract::{BoxFuture, ToolCtx};
use serde::{Deserialize, Serialize};

use super::provider::{BackupOutcome, BackupProvider};

/// The `backup`/`host` config row shape. All fields optional so a partial row is
/// valid and an absent row yields defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostBackupConfig {
    /// Absolute paths to capture. Empty → [`default_includes`].
    #[serde(default)]
    pub include: Vec<String>,
    /// Absolute path prefixes to skip within the includes.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Reserved: commit the backup target to git for off-host recovery. Not yet
    /// honored — TODO wire git-commit once the store target is git-backed.
    #[serde(default)]
    pub git_commit: bool,
    /// Reserved: an explicit backup store target path override. Not yet honored.
    #[serde(default)]
    pub target: Option<String>,
}

const NOUN: &str = "backup";
const NAME: &str = "host";

/// orca's own config paths under the state dir, filtered to those that exist.
/// Deliberately excludes `orca.db` and `logs/` (local history, not portable
/// config).
pub fn default_includes() -> Vec<PathBuf> {
    let Ok(state) = contract::config::state_dir() else {
        return Vec::new();
    };
    ["orca.toml", "pki", "profiles", "memory"]
        .iter()
        .map(|p| state.join(p))
        .filter(|p| p.exists())
        .collect()
}

/// Paths always skipped, on top of any config `exclude`. The mesh LEAF certs
/// (`pki/mesh/{server,client}/node.{cert,key}.pem`) are DERIVED identity, not
/// portable config: the pod runtime re-mints them from the mesh CA on every
/// daemon start when their CN doesn't match this host
/// ([[data-classification-config-syncs-history-local]]). Capturing them makes a
/// host backup that can never round-trip byte-for-byte (the live daemon
/// overwrites the restored leaves), so they are excluded at capture. The mesh CA
/// and everything else under `pki/` — the actual root of trust — is still
/// captured, and the leaves re-derive from it on restore.
pub fn default_excludes() -> Vec<PathBuf> {
    let Ok(state) = contract::config::state_dir() else {
        return Vec::new();
    };
    let pki = state.join("pki");
    vec![
        utils::pki::mesh_server_cert_path(&pki),
        utils::pki::mesh_server_key_path(&pki),
        utils::pki::mesh_client_cert_path(&pki),
        utils::pki::mesh_client_key_path(&pki),
    ]
}

/// The `host` provider.
#[derive(Debug, Default)]
pub struct HostBackupProvider;

impl HostBackupProvider {
    pub fn new() -> Self {
        Self
    }
}

impl BackupProvider for HostBackupProvider {
    fn kind(&self) -> &str {
        "host"
    }

    fn title(&self) -> &str {
        "Host configuration"
    }

    /// `hosts/[<placement-label>/]<hostname>` — a host backup files under the
    /// generic `hosts` category, self-namespaced by hostname (which keeps fleet
    /// host backups from colliding). If a plugin has tagged this host with a
    /// placement label (e.g. the Proxmox plugin writing `"proxmox"`), it is
    /// inserted as a grouping class; with no label the layout is `hosts/<hostname>`.
    /// Core assigns NO placement label itself ([[orca-core-generic-plugins-expose-functionality]]).
    fn layout(&self, _instance: &str) -> Vec<String> {
        let mut segs = vec!["hosts".to_string()];
        if let Some(class) = super::target::placement().labels.into_iter().next() {
            segs.push(class);
        }
        segs.push(crate::host_identity::cli_hostname_or_fallback());
        segs
    }

    fn backup<'a>(
        &'a self,
        payload_dir: &'a Path,
        _instance: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<BackupOutcome>> {
        Box::pin(async move {
            let cfg = load_config();
            let includes: Vec<PathBuf> = if cfg.include.is_empty() {
                default_includes()
            } else {
                cfg.include.iter().map(PathBuf::from).collect()
            };
            // Config excludes plus the always-skip derived paths (mesh leaf
            // certs) — the latter must never be captured regardless of config.
            let mut excludes: Vec<PathBuf> = cfg.exclude.iter().map(PathBuf::from).collect();
            excludes.extend(default_excludes());
            do_backup(&includes, &excludes, payload_dir)
        })
    }

    fn restore<'a>(
        &'a self,
        payload_dir: &'a Path,
        _instance: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { do_restore(payload_dir) })
    }
}

/// Load the `backup`/`host` config row, falling back to defaults on any DB or
/// parse error so a host backup never fails just because config is unavailable.
fn load_config() -> HostBackupConfig {
    let read = db::pool::with_pooled_or_open(|conn| db::config_store::get(conn, NOUN, NAME));
    match read {
        Ok(Some(row)) => match serde_json::from_str::<HostBackupConfig>(&row.json) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("[backup:host] bad {NOUN}/{NAME} config, using defaults: {e}");
                HostBackupConfig::default()
            }
        },
        Ok(None) => HostBackupConfig::default(),
        Err(e) => {
            tracing::warn!("[backup:host] cannot read {NOUN}/{NAME} config, using defaults: {e}");
            HostBackupConfig::default()
        }
    }
}

/// Copy every include (minus excludes) into `payload_dir`, mirroring absolute
/// paths. Returns the captured note.
fn do_backup(
    includes: &[PathBuf],
    excludes: &[PathBuf],
    payload_dir: &Path,
) -> Result<BackupOutcome> {
    let mut captured = Vec::new();
    for src in includes {
        if !src.exists() {
            tracing::warn!(
                "[backup:host] include path missing, skipping: {}",
                src.display()
            );
            continue;
        }
        let dest = mirror_dest(payload_dir, src)?;
        copy_path(src, &dest, excludes)?;
        captured.push(src.to_string_lossy().into_owned());
    }
    Ok(BackupOutcome {
        checksum: None,
        note: Some(format!("host paths: {}", captured.join(", "))),
    })
}

/// Restore: walk the payload mirror and copy each file back to `/` + its path
/// relative to the payload root.
fn do_restore(payload_dir: &Path) -> Result<()> {
    let mut stack = vec![payload_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("read payload dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(payload_dir)
                    .with_context(|| format!("payload path outside root: {}", path.display()))?;
                let target = PathBuf::from("/").join(rel);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("mkdir {}", parent.display()))?;
                }
                std::fs::copy(&path, &target).with_context(|| {
                    format!("restore {} -> {}", path.display(), target.display())
                })?;
            }
        }
    }
    Ok(())
}

/// The mirror destination under `payload_dir` for an absolute `src`
/// (`/a/b` → `<payload>/a/b`). Errors on a non-absolute source.
fn mirror_dest(payload_dir: &Path, src: &Path) -> Result<PathBuf> {
    let rel = src
        .strip_prefix("/")
        .with_context(|| format!("backup source must be an absolute path: {}", src.display()))?;
    Ok(payload_dir.join(rel))
}

/// True if `path` is at or under any exclude prefix.
fn is_excluded(path: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|ex| path.starts_with(ex))
}

/// Recursively copy `src` to `dest`, skipping excluded paths. `src` may be a
/// file or a directory.
fn copy_path(src: &Path, dest: &Path, excludes: &[PathBuf]) -> Result<()> {
    if is_excluded(src, excludes) {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(dest).with_context(|| format!("mkdir {}", dest.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry?;
            let child = entry.path();
            if is_excluded(&child, excludes) {
                continue;
            }
            copy_path(&child, &dest.join(entry.file_name()), excludes)?;
        }
    } else {
        // Regular file (symlinks are copied by content — a host config backup
        // wants the pointed-at data, not a dangling link).
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::copy(src, dest)
            .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn backup_mirrors_absolute_paths_and_honors_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        // A source tree with a file to keep and a subdir to exclude.
        let src_root = tmp.path().join("src").join("cfg");
        fs::create_dir_all(src_root.join("keep")).unwrap();
        fs::create_dir_all(src_root.join("skip")).unwrap();
        fs::write(src_root.join("orca.toml"), "a=1").unwrap();
        fs::write(src_root.join("keep").join("k.txt"), "keep").unwrap();
        fs::write(src_root.join("skip").join("s.txt"), "skip").unwrap();

        let payload = tmp.path().join("payload");
        fs::create_dir_all(&payload).unwrap();

        let excludes = vec![src_root.join("skip")];
        let outcome = do_backup(std::slice::from_ref(&src_root), &excludes, &payload).unwrap();
        assert!(outcome.note.is_some(), "note present");

        // Mirror = payload + absolute(src) with leading '/' stripped.
        let mirrored = payload.join(src_root.strip_prefix("/").unwrap());
        assert!(mirrored.join("orca.toml").exists());
        assert!(mirrored.join("keep").join("k.txt").exists());
        assert!(!mirrored.join("skip").exists(), "excluded subdir omitted");
    }

    #[test]
    fn missing_include_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        let missing = tmp.path().join("does-not-exist");
        // Should succeed, capturing nothing.
        let out = do_backup(&[missing], &[], &payload).unwrap();
        assert_eq!(out.note.as_deref(), Some("host paths: "));
    }

    #[test]
    fn backup_then_restore_round_trips() {
        // Round-trip through the mirror: back up a tree, wipe it, restore.
        let tmp = tempfile::tempdir().unwrap();
        // Use a source under the tempdir so restore (which writes to '/'+rel)
        // lands back exactly where it came from.
        let src = tmp.path().join("data");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("top.txt"), "top").unwrap();
        fs::write(src.join("sub").join("nested.txt"), "nested").unwrap();

        let payload = tmp.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        do_backup(std::slice::from_ref(&src), &[], &payload).unwrap();

        // Wipe the originals.
        fs::remove_dir_all(&src).unwrap();
        assert!(!src.exists());

        // Restore copies each payload file back to '/' + its relative path,
        // which reconstructs the absolute tempdir paths.
        do_restore(&payload).unwrap();
        assert_eq!(fs::read_to_string(src.join("top.txt")).unwrap(), "top");
        assert_eq!(
            fs::read_to_string(src.join("sub").join("nested.txt")).unwrap(),
            "nested"
        );
    }

    #[test]
    fn mirror_dest_requires_absolute() {
        let payload = Path::new("/tmp/payload");
        assert!(mirror_dest(payload, Path::new("relative/path")).is_err());
        assert_eq!(
            mirror_dest(payload, Path::new("/a/b/c")).unwrap(),
            PathBuf::from("/tmp/payload/a/b/c")
        );
    }

    #[test]
    #[serial_test::serial(env)]
    fn default_includes_filters_to_existing() {
        // Point ORCA_HOME at a temp state dir with only some of the expected entries.
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(state.join("pki")).unwrap();
        fs::write(state.join("orca.toml"), "x=1").unwrap();
        // no profiles/, no memory/, no orca.db

        // SAFETY: single-threaded test env mutation; scoped and restored.
        let prev = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", &state) };
        let inc = default_includes();
        match prev {
            Some(v) => unsafe { std::env::set_var("ORCA_HOME", v) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }

        let names: Vec<String> = inc
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"orca.toml".to_string()));
        assert!(names.contains(&"pki".to_string()));
        assert!(!names.contains(&"profiles".to_string()));
        assert!(!names.contains(&"memory".to_string()));
    }

    #[test]
    #[serial_test::serial(env)]
    fn default_excludes_skip_mesh_leaf_certs_but_keep_ca() {
        // A pki tree under a temp state dir: mesh CA + server/client leaf pairs.
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let pki = state.join("pki");
        let mesh = pki.join("mesh");
        fs::create_dir_all(mesh.join("server")).unwrap();
        fs::create_dir_all(mesh.join("client")).unwrap();
        fs::write(mesh.join("ca.cert.pem"), "ca").unwrap();
        fs::write(mesh.join("server").join("node.cert.pem"), "sc").unwrap();
        fs::write(mesh.join("server").join("node.key.pem"), "sk").unwrap();
        fs::write(mesh.join("client").join("node.cert.pem"), "cc").unwrap();
        fs::write(mesh.join("client").join("node.key.pem"), "ck").unwrap();

        // SAFETY: single-threaded test env mutation; scoped and restored.
        let prev = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", &state) };
        let excludes = default_excludes();

        let payload = tmp.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        do_backup(std::slice::from_ref(&pki), &excludes, &payload).unwrap();

        match prev {
            Some(v) => unsafe { std::env::set_var("ORCA_HOME", v) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }

        let mirrored = payload.join(pki.strip_prefix("/").unwrap());
        // CA is kept — the root of trust is portable config.
        assert!(mirrored.join("mesh/ca.cert.pem").exists(), "CA captured");
        // Leaf certs/keys are derived identity — never captured.
        assert!(!mirrored.join("mesh/server/node.cert.pem").exists());
        assert!(!mirrored.join("mesh/server/node.key.pem").exists());
        assert!(!mirrored.join("mesh/client/node.cert.pem").exists());
        assert!(!mirrored.join("mesh/client/node.key.pem").exists());
    }

    #[test]
    fn config_defaults_deserialize() {
        let cfg: HostBackupConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.include.is_empty());
        assert!(cfg.exclude.is_empty());
        assert!(!cfg.git_commit);
        assert!(cfg.target.is_none());

        let cfg: HostBackupConfig = serde_json::from_str(
            r#"{"include":["/a"],"exclude":["/a/skip"],"gitCommit":true,"target":"/mnt/b"}"#,
        )
        .unwrap();
        // camelCase not enforced here (serde default snake); accept snake in row.
        assert_eq!(cfg.include, vec!["/a".to_string()]);
    }
}
