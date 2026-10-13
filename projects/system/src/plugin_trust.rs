//! Daemon side of plugin trust: installing a plugin is the trust exchange.
//!
//! Plugin install/update/delete are admin-only, so when one succeeds the daemon
//! records (or drops) the binary's sha256 in the root-owned store that
//! `admin plugin-apply` checks. A non-root daemon goes through `sudo -n <orca>
//! admin plugin-trust|plugin-untrust` with the request on stdin; a root daemon
//! writes the store in-process.
//!
//! The hash of every binary the daemon installs is also kept under
//! `<orca_home>/plugin-hashes`, so [`reconcile_on_start`] can re-record trust
//! after an orca upgrade or a reboot that wiped a RAM-backed `/etc` (Unraid).
//! A binary whose hash no longer matches what the daemon installed is never
//! re-trusted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::plugin_apply::{self, TrustRequest, TrustStore};

/// How trust records are written on this host.
pub enum Seam {
    /// The daemon is root: write `store` directly, binaries owned by `owner_uid`.
    InProcess { store: TrustStore, owner_uid: u32 },
    /// `sudo -n <orca> admin plugin-trust|plugin-untrust`, `orca` being the
    /// canonical path the sudoers grant names.
    Sudo { orca: PathBuf },
}

impl Seam {
    pub fn for_daemon() -> Result<Self> {
        let exe = std::env::current_exe().context("resolve the orca binary path")?;
        let exe = exe.canonicalize().unwrap_or(exe);
        // Only the orca binary holds the sudoers grant; anything else (a test
        // harness linking this crate) must not touch the system store.
        ensure!(
            exe.file_name().is_some_and(|f| f == "orca"),
            "{} is not the orca binary",
            exe.display()
        );
        if plugin_apply::is_root() {
            // SAFETY: geteuid has no preconditions and cannot fail.
            let owner_uid = unsafe { libc::geteuid() };
            return Ok(Self::InProcess {
                store: TrustStore::system(),
                owner_uid,
            });
        }
        Ok(Self::Sudo { orca: exe })
    }

    async fn trust(&self, home: &Path, name: &str, sha256: &str) -> Result<()> {
        match self {
            Self::InProcess { store, owner_uid } => {
                plugin_apply::trust(home, store, name, *owner_uid, sha256).map(drop)
            }
            Self::Sudo { orca } => {
                let req = TrustRequest {
                    name: name.into(),
                    sha256: Some(sha256.into()),
                };
                run_sudo(orca, "plugin-trust", &req).await
            }
        }
    }

    async fn untrust(&self, name: &str) -> Result<()> {
        match self {
            Self::InProcess { store, .. } => store.remove(name),
            Self::Sudo { orca } => {
                let req = TrustRequest {
                    name: name.into(),
                    sha256: None,
                };
                run_sudo(orca, "plugin-untrust", &req).await
            }
        }
    }
}

async fn run_sudo(orca: &Path, verb: &str, req: &TrustRequest) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("sudo")
        .arg("-n")
        .arg(orca)
        .args(["admin", verb])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn `sudo -n {} admin {verb}`", orca.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&serde_json::to_vec(req)?).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    ensure!(
        out.status.success(),
        "`sudo -n {} admin {verb}` failed ({}): {}",
        orca.display(),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

fn hash_path(home: &Path, name: &str) -> PathBuf {
    home.join("plugin-hashes").join(format!("{name}.sha256"))
}

/// The hash the daemon recorded when it installed `name`.
pub fn installed_hash(home: &Path, name: &str) -> Option<String> {
    let hex = std::fs::read_to_string(hash_path(home, name)).ok()?;
    Some(hex.trim().to_string())
}

/// What an install/uninstall did about trust, for the operator-facing result.
#[derive(Debug, Default)]
pub struct TrustOutcome {
    pub recorded: bool,
    pub warning: Option<String>,
}

/// After an admin installed `name` at `binary`: remember its hash and record
/// it as trusted. A failure is returned as a warning, never an error, because
/// the install itself succeeded.
pub async fn after_install(seam: &Seam, home: &Path, name: &str, binary: &Path) -> TrustOutcome {
    let res = async {
        plugin_apply::validate_plugin_name(name)?;
        let hex = utils::hash::sha256_hex(
            &std::fs::read(binary).with_context(|| format!("read {}", binary.display()))?,
        );
        let path = hash_path(home, name);
        files::ops::mkdir_p(path.parent().context("hash dir")?)?;
        std::fs::write(&path, format!("{hex}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        seam.trust(home, name, &hex).await
    }
    .await;
    match res {
        Ok(()) => TrustOutcome {
            recorded: true,
            warning: None,
        },
        Err(e) => {
            let warning = format!(
                "plugin '{name}' is installed, but its trust record could not be written, so \
                 its privileged ops will be refused: {e:#}"
            );
            tracing::warn!(plugin = %name, error = %format!("{e:#}"), "plugin trust not recorded");
            TrustOutcome {
                recorded: false,
                warning: Some(warning),
            }
        }
    }
}

/// After an admin uninstalled `name`: forget its hash and drop its trust record.
pub async fn after_uninstall(seam: &Seam, home: &Path, name: &str) -> TrustOutcome {
    match std::fs::remove_file(hash_path(home, name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!(plugin = %name, error = %e, "could not remove installed plugin hash");
        }
        _ => {}
    }
    match seam.untrust(name).await {
        Ok(()) => TrustOutcome {
            recorded: true,
            warning: None,
        },
        Err(e) => {
            tracing::warn!(plugin = %name, error = %format!("{e:#}"), "plugin trust not removed");
            TrustOutcome {
                recorded: false,
                warning: Some(format!(
                    "plugin '{name}' is uninstalled, but its trust record could not be removed: {e:#}"
                )),
            }
        }
    }
}

/// Re-record trust for every plugin the daemon installed whose binary still
/// hashes to what it installed. Records already current are left alone, so a
/// steady-state start runs no `sudo`. Returns the names it recorded.
pub async fn reconcile(seam: &Seam, home: &Path, current: &TrustStore) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join("plugin-hashes")) else {
        return Vec::new();
    };
    let mut recorded = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".sha256")) else {
            continue;
        };
        if plugin_apply::validate_plugin_name(name).is_err() {
            continue;
        }
        let Some(want) = installed_hash(home, name) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(home.join("plugins").join(name)) else {
            continue;
        };
        if utils::hash::sha256_hex(&bytes) != want {
            tracing::warn!(
                plugin = %name,
                "installed plugin binary differs from what orca installed; not trusting it"
            );
            continue;
        }
        if current.expected_hash(name).ok().as_deref() == Some(want.as_str()) {
            continue;
        }
        match seam.trust(home, name, &want).await {
            Ok(()) => recorded.push(name.to_string()),
            Err(e) => tracing::warn!(
                plugin = %name,
                error = %format!("{e:#}"),
                "could not re-record plugin trust; its privileged ops will be refused"
            ),
        }
    }
    recorded
}

/// Daemon-start pass: [`reconcile`] against the system store.
pub async fn reconcile_on_start() {
    let Some(home) = files::ops::orca_home() else {
        return;
    };
    let seam = match Seam::for_daemon() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "plugin trust reconcile skipped");
            return;
        }
    };
    let recorded = reconcile(&seam, &home, &TrustStore::system()).await;
    if !recorded.is_empty() {
        tracing::info!(plugins = ?recorded, "re-recorded plugin trust");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn setup() -> (tempfile::TempDir, TrustStore) {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("plugins")).unwrap();
        let store = TrustStore {
            dir: home.path().join("etc/orca/plugin-trust"),
            anchor: home.path().to_path_buf(),
            owner_uid: me(),
        };
        (home, store)
    }

    fn seam(store: &TrustStore) -> Seam {
        Seam::InProcess {
            store: store.clone(),
            owner_uid: me(),
        }
    }

    fn put(home: &Path, name: &str, body: &str) -> PathBuf {
        let p = home.join("plugins").join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn a_non_orca_binary_never_gets_a_seam() {
        let err = Seam::for_daemon().err().expect("test harness is not orca");
        assert!(
            err.to_string().contains("is not the orca binary"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn install_records_trust_and_uninstall_drops_it() {
        let (home, store) = setup();
        let bin = put(home.path(), "unraid", "v1");
        let out = after_install(&seam(&store), home.path(), "unraid", &bin).await;
        assert!(out.recorded && out.warning.is_none(), "{:?}", out.warning);
        let hex = utils::hash::sha256_hex(b"v1");
        assert_eq!(store.expected_hash("unraid").unwrap(), hex);
        assert_eq!(installed_hash(home.path(), "unraid").unwrap(), hex);

        let out = after_uninstall(&seam(&store), home.path(), "unraid").await;
        assert!(out.recorded);
        assert!(store.expected_hash("unraid").is_err());
        assert!(installed_hash(home.path(), "unraid").is_none());
    }

    #[tokio::test]
    async fn a_failed_record_says_privileged_ops_will_be_refused() {
        let (home, store) = setup();
        let bin = put(home.path(), "unraid", "v1");
        let foreign = TrustStore {
            owner_uid: me().wrapping_add(1),
            ..store
        };
        let out = after_install(&seam(&foreign), home.path(), "unraid", &bin).await;
        assert!(!out.recorded);
        let w = out.warning.unwrap();
        assert!(
            w.contains("is installed") && w.contains("will be refused"),
            "{w}"
        );
    }

    #[tokio::test]
    async fn the_sudo_seam_failing_is_reported_not_silent() {
        let (home, _) = setup();
        let bin = put(home.path(), "unraid", "v1");
        let seam = Seam::Sudo {
            orca: PathBuf::from("/nonexistent/orca"),
        };
        let out = after_install(&seam, home.path(), "unraid", &bin).await;
        assert!(!out.recorded);
        assert!(out.warning.unwrap().contains("will be refused"));
    }

    #[tokio::test]
    async fn reconcile_rerecords_what_orca_installed_and_nothing_else() {
        let (home, store) = setup();
        let bin = put(home.path(), "kept", "v1");
        after_install(&seam(&store), home.path(), "kept", &bin).await;
        let bin = put(home.path(), "swapped", "v1");
        after_install(&seam(&store), home.path(), "swapped", &bin).await;
        put(home.path(), "sideways", "never installed by orca");

        // A reboot wiped the RAM-backed store.
        std::fs::remove_dir_all(&store.dir).unwrap();
        put(home.path(), "swapped", "tampered");

        let recorded = reconcile(&seam(&store), home.path(), &store).await;
        assert_eq!(recorded, vec!["kept".to_string()]);
        assert!(store.expected_hash("swapped").is_err());
        assert!(store.expected_hash("sideways").is_err());

        // Already current: nothing to do.
        assert!(
            reconcile(&seam(&store), home.path(), &store)
                .await
                .is_empty()
        );
    }
}
