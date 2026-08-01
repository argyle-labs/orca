//! Backup TARGET provider trait + process-global registry — the WHERE axis,
//! orthogonal to the provider KIND (the WHAT, see [`super::provider`]).
//!
//! A [`BackupTargetProvider`] knows one target KIND (`local`, or a plugin's
//! `nfs`/`smb`/`s3`/`pbs`/`git`) and resolves a named target instance to a
//! concrete filesystem-rooted [`BackupStore`] — the generic store then owns
//! layout, listing, selection, and retention beneath it, identically for every
//! target. Push/pull backings (git, s3, pbs) stage to a local working tree via
//! [`open`](BackupTargetProvider::open) and reconcile the remote in the
//! [`sync`](BackupTargetProvider::sync) / [`refresh`](BackupTargetProvider::refresh)
//! lifecycle hooks.
//!
//! **Core owns exactly ONE target kind: the built-in `local` file-path target**
//! ([[orca-core-generic-plugins-expose-functionality]]). Every other kind is
//! plugin-exposed and registered here at load, exactly as plugins register
//! backup KINDs ([[no-kind-owned-by-plugin]], [[always-hunt-abstractions-reuse-seams]]).
//!
//! No `async_trait` macro ([[no-async-trait-macro]]): async methods return the
//! hand-desugared [`contract::BoxFuture`], as the sibling registries do.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use contract::backup::Placement;
use contract::{BoxFuture, ToolCtx};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::store::BackupStore;

/// A concrete storage location a target kind exposes for selection — the "point
/// a target" surface. A storage plugin (smb/nfs) enumerates the mounts/shares it
/// manages; the backup-create flow lists these so the user picks the ROOT (e.g.
/// the smb `/backups` mount). The sub-path beneath is the provider-declared
/// taxonomy by default ([[orca-must-be-declarative-config-driven]]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetLocation {
    /// Stable id within the kind — becomes the target ref `name` when selected.
    pub id: String,
    /// Human label for the picker (e.g. `SMB //nas/backups`).
    pub label: String,
    /// The base filesystem path this location roots at, when it is a mounted /
    /// local path. Absent for object stores addressed by key (s3) — those carry
    /// the address in [`backing_key`](Self::backing_key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_path: Option<String>,
    /// GLOBALLY STABLE identity of the underlying storage, used for FLEET-WIDE
    /// collision detection: two hosts collide only when they write the same
    /// `backing_key` + overlapping sub-path. Per-host local disks are namespaced
    /// (`local://<host>`) so they never collide cross-host; shared backings carry
    /// their shared address (`nfs://server/export`, `s3://bucket`).
    pub backing_key: String,
}

/// One backup TARGET kind. `open` resolves a named target instance to a store
/// (provisioning the directory / mount / clone as needed); `sync`/`refresh` are
/// the post-write / pre-read reconciliation hooks a remote backing needs (git
/// push/pull, s3 upload/download) — no-ops for a plain local path.
pub trait BackupTargetProvider: Send + Sync {
    /// Target kind name (`"local"`, `"nfs"`, …). Unique across the registry; it
    /// is the [`BackupTargetRef::kind`](contract::backup::BackupTargetRef::kind).
    fn kind(&self) -> &str;

    /// Human-facing title for listings. Defaults to the kind.
    fn title(&self) -> &str {
        self.kind()
    }

    /// Resolve the named target instance to a filesystem-rooted store,
    /// provisioning storage as needed. `name` disambiguates multiple targets of
    /// this kind (`default` for the single, unnamed one).
    fn open<'a>(&'a self, name: &'a str, ctx: &'a ToolCtx) -> BoxFuture<'a, Result<BackupStore>>;

    /// Push local writes to the remote backing after a run committed to the store
    /// (git commit+push, s3 upload). Default: nothing to do (a plain local path
    /// is already durable). Never fatal to a backup that already committed.
    fn sync<'a>(&'a self, _name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Pull the remote backing into the local working tree before a list/restore
    /// reads it (git pull, s3 download). Default: nothing to do.
    fn refresh<'a>(&'a self, _name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Whether this target is eligible for a workload at `placement`. The default
    /// fits everywhere (as `local` does); a placement-sensitive target (PBS)
    /// overrides to require Proxmox. Only gates what is OFFERED — never a user's
    /// explicit choice.
    fn fits(&self, _placement: &Placement) -> bool {
        true
    }

    /// The concrete storage locations this kind exposes for selection (the mounts
    /// an smb/nfs plugin manages, the buckets an s3 plugin knows). The
    /// backup-create flow lists these to let the user point a target at a root.
    /// Default: none enumerable (a kind with no discoverable locations).
    fn available<'a>(&'a self, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Vec<TargetLocation>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// The globally stable backing identity for the named target instance, used
    /// for FLEET-WIDE collision detection (see [`TargetLocation::backing_key`]).
    /// Default `<kind>://<name>`; a target with per-host or shared storage MUST
    /// override so cross-host comparison is meaningful (`local://<host>` for a
    /// per-host disk, `nfs://server/export` for a shared export).
    fn backing_key<'a>(
        &'a self,
        name: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<String>> {
        let key = format!("{}://{}", self.kind(), name);
        Box::pin(async move { Ok(key) })
    }
}

/// Detect where this host sits, to offer placement-appropriate targets and to
/// label host backups by placement class. Proxmox is signalled by the PVE config
/// dir or a wired PBS repository/storage (the same cheap env/file probe the
/// `service` crate's `pbs_available` uses).
pub(crate) fn detect_placement() -> Placement {
    let proxmox = std::path::Path::new("/etc/pve").is_dir()
        || std::env::var_os("PBS_REPOSITORY").is_some()
        || std::env::var_os("ORCA_PBS_STORAGE").is_some();
    Placement {
        host: None,
        proxmox,
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn BackupTargetProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register (or replace, by kind name) a backup target provider.
pub fn register_target(target: Arc<dyn BackupTargetProvider>) {
    let mut g = GLOBAL.write().expect("backup target registry poisoned");
    let kind = target.kind().to_string();
    if let Some(slot) = g.iter_mut().find(|t| t.kind() == kind) {
        *slot = target;
    } else {
        g.push(target);
    }
}

/// Every registered target provider.
pub fn targets() -> Vec<Arc<dyn BackupTargetProvider>> {
    GLOBAL
        .read()
        .expect("backup target registry poisoned")
        .clone()
}

/// The target provider for `kind`, if one is registered.
pub fn target(kind: &str) -> Option<Arc<dyn BackupTargetProvider>> {
    GLOBAL
        .read()
        .expect("backup target registry poisoned")
        .iter()
        .find(|t| t.kind() == kind)
        .cloned()
}

/// Remove the target provider for `kind`. Returns true if one was removed.
/// Mainly for tests; production only ever registers.
pub fn deregister_target(kind: &str) -> bool {
    let mut g = GLOBAL.write().expect("backup target registry poisoned");
    let before = g.len();
    g.retain(|t| t.kind() != kind);
    before != g.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTarget {
        kind: String,
        proxmox_only: bool,
    }

    impl BackupTargetProvider for FakeTarget {
        fn kind(&self) -> &str {
            &self.kind
        }
        fn open<'a>(
            &'a self,
            _name: &'a str,
            _ctx: &'a ToolCtx,
        ) -> BoxFuture<'a, Result<BackupStore>> {
            Box::pin(async { Ok(BackupStore::new("/tmp/fake")) })
        }
        fn fits(&self, placement: &Placement) -> bool {
            !self.proxmox_only || placement.proxmox
        }
    }

    #[test]
    fn register_lookup_replace_deregister() {
        let kind = "fake-target-kind";
        deregister_target(kind);
        assert!(target(kind).is_none());

        register_target(Arc::new(FakeTarget {
            kind: kind.into(),
            proxmox_only: false,
        }));
        let t = target(kind).expect("registered");
        assert_eq!(t.kind(), kind);
        assert_eq!(t.title(), kind, "title defaults to kind");

        // Re-register replaces, never duplicates.
        register_target(Arc::new(FakeTarget {
            kind: kind.into(),
            proxmox_only: false,
        }));
        assert_eq!(targets().iter().filter(|t| t.kind() == kind).count(), 1);

        assert!(deregister_target(kind));
        assert!(target(kind).is_none());
    }

    #[test]
    fn fits_gates_offering_by_placement() {
        let pbs_like = FakeTarget {
            kind: "pbs-like".into(),
            proxmox_only: true,
        };
        assert!(!pbs_like.fits(&Placement::bare()));
        assert!(pbs_like.fits(&Placement::proxmox()));

        let anywhere = FakeTarget {
            kind: "anywhere".into(),
            proxmox_only: false,
        };
        assert!(anywhere.fits(&Placement::bare()));
    }
}
