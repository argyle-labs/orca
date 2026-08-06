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
use contract::backup::{BackupSchedule, Placement, Retention};
use contract::{BoxFuture, ToolCtx};

use super::store::BackupStore;

/// A concrete storage location a target kind exposes for selection — the "point
/// a target" surface. The type lives in `contract` so an out-of-process
/// backup-TARGET plugin can return it across the JSON-proxy boundary;
/// re-exported here for the in-crate target API.
pub use contract::backup::TargetLocation;

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
    /// fits everywhere (as `local` does); a placement-sensitive target overrides
    /// to require a matching label. Only gates what is OFFERED — never a user's
    /// explicit choice.
    fn fits(&self, _placement: &Placement) -> bool {
        true
    }

    /// This target's default retention, applied to backups written here when a
    /// binding sets none. `None` falls through to the unit's policy default.
    fn default_retention(&self, _name: &str) -> Option<Retention> {
        None
    }

    /// This target's default schedule, applied to backups written here when a
    /// binding sets none. `None` falls through to the unit's policy default.
    fn default_schedule(&self, _name: &str) -> Option<BackupSchedule> {
        None
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

/// This host's placement — a set of OPAQUE, plugin-assigned labels read from the
/// `backup`/`placement` config row ([[orca-must-be-declarative-config-driven]]).
///
/// Core detects NOTHING platform-specific: it never probes for Proxmox or any
/// other platform ([[orca-core-generic-plugins-expose-functionality]]). A plugin
/// that manages a platform writes the label it owns (e.g. the Proxmox plugin
/// tags its hosts `"proxmox"`); with no plugin/config the placement is bare. A
/// target's `fits()` is the only code that interprets a label.
pub(crate) fn placement() -> Placement {
    #[derive(serde::Deserialize, Default)]
    struct PlacementRow {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
    }
    let read =
        db::pool::with_pooled_or_open(|conn| db::config_store::get(conn, "backup", "placement"));
    match read {
        Ok(Some(row)) => match serde_json::from_str::<PlacementRow>(&row.json) {
            Ok(p) => Placement {
                host: p.host,
                labels: p.labels,
            },
            Err(e) => {
                tracing::warn!("[backup] bad backup/placement config, using bare: {e}");
                Placement::bare()
            }
        },
        Ok(None) => Placement::bare(),
        Err(e) => {
            tracing::warn!("[backup] cannot read backup/placement config, using bare: {e}");
            Placement::bare()
        }
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

/// Remove the target provider for `kind`, so the surfaces stop offering it.
/// Returns true if one was removed.
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
        /// A placement label this target requires to be offered (a plugin's own
        /// concept — core assigns no meaning); `None` fits everywhere.
        required_label: Option<String>,
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
            match &self.required_label {
                Some(l) => placement.has(l),
                None => true,
            }
        }
    }

    #[test]
    fn register_lookup_replace_deregister() {
        let kind = "fake-target-kind";
        deregister_target(kind);
        assert!(target(kind).is_none());

        register_target(Arc::new(FakeTarget {
            kind: kind.into(),
            required_label: None,
        }));
        let t = target(kind).expect("registered");
        assert_eq!(t.kind(), kind);
        assert_eq!(t.title(), kind, "title defaults to kind");

        // Re-register replaces, never duplicates.
        register_target(Arc::new(FakeTarget {
            kind: kind.into(),
            required_label: None,
        }));
        assert_eq!(targets().iter().filter(|t| t.kind() == kind).count(), 1);

        assert!(deregister_target(kind));
        assert!(target(kind).is_none());
    }

    #[test]
    fn fits_gates_offering_by_placement_label() {
        // A target that only fits where a plugin-assigned label is present.
        let label_gated = FakeTarget {
            kind: "label-gated".into(),
            required_label: Some("some-platform".into()),
        };
        assert!(!label_gated.fits(&Placement::bare()));
        assert!(label_gated.fits(&Placement::with_labels(["some-platform".to_string()])));

        let anywhere = FakeTarget {
            kind: "anywhere".into(),
            required_label: None,
        };
        assert!(anywhere.fits(&Placement::bare()));
    }
}
