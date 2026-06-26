//! Generic storage domain. One model, one adapter trait, one registry — many
//! backends (NFS, SMB, Proxmox-managed disk storage, …).
//!
//! orca does not care *what kind* of storage a provider is; it cares that it
//! has access to storage and what that storage can do. A plugin contributes
//! facts ("this share exists, it is mountable on host X") and capabilities
//! ("I can mount/unmount/list"). Consumers (the topology aggregator, the
//! self-healing mount reconciler, `storage.*` tools) iterate the registered
//! backends rather than reaching for `nfs`/`smb`/`proxmox` by name.
//!
//! Follows the same plug-in shape as `notifications` and `containers`:
//! a [`StorageBackend`] trait + a process-global registry every adapter
//! registers itself against at bootstrap.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, RwLock};
use thiserror::Error;

/// Cross-platform kernel-mount-table primitive shared by every network-share
/// backend (nfs, smb, …). Plugins read the live table and classify health
/// through this rather than each parsing `/proc/mounts` themselves.
pub mod mount_table;

pub use mount_table::{Health, MountEntry, mount_table, mount_table_of, probe_health};

// ── Model ───────────────────────────────────────────────────────────────────

/// The flavour of storage a backend provides. Deliberately coarse — consumers
/// branch on capability, not kind. Kind exists for display + topology grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// Network file share mounted over the network (NFS export, SMB/CIFS share).
    NetworkShare,
    /// Host-local / hypervisor-managed disk storage (Proxmox storage pools,
    /// LVM, ZFS, directory). Has no network-share semantics of its own but can
    /// be enumerated and have its usage reported via an API.
    DiskStorage,
    /// Object storage (S3-compatible). Reserved for future adapters.
    Object,
}

/// A capability a backend supports. Consumers check these before invoking an
/// operation so an unsupported call fails fast rather than at the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Enumerate the shares/volumes this backend exposes.
    List,
    /// Mount a share onto a target path on a host.
    Mount,
    /// Unmount a previously-mounted share (incl. lazy/forced recovery).
    Unmount,
    /// Report capacity/usage for a volume.
    Usage,
    /// Create a new share/volume.
    Create,
    /// Remove a share/volume.
    Remove,
    /// Probe for and self-heal stale / vanished mounts (lazy-release + remount).
    RecoverStale,
}

/// Outcome of a [`StorageBackend::recover_stale`] sweep: a stale-mount
/// health-probe → force-release → remount → re-probe cycle, plus recovery of
/// declared-but-absent mounts. The reconciler logs this and continues its own
/// recovery (e.g. a hypervisor lifecycle restart) regardless of the result.
///
/// Domain-owned so consumers (proxmox's wedge recovery) depend only on the
/// `storage` domain, never on a concrete network-share backend.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RecoverOutcome {
    /// Mountpoints that were stale on the first probe and healthy after recovery.
    pub recovered: Vec<String>,
    /// Mountpoints still unhealthy after the recovery sequence.
    pub still_stale: Vec<String>,
    /// Mountpoints declared but absent that were successfully remounted.
    pub remounted: Vec<String>,
    /// Declared-but-absent mountpoints that could not be remounted.
    pub still_missing: Vec<String>,
    /// Non-fatal errors encountered during recovery.
    pub errors: Vec<String>,
    /// `true` when nothing was stale and nothing was missing (fast path / no-op).
    pub no_stale_found: bool,
}

/// A storage provider as registered with orca: a named backend, its kind, and
/// the capabilities it advertises. This is the row `storage.list` surfaces and
/// the topology aggregator turns into nodes/edges.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Provider {
    /// Unique provider name (matches [`StorageBackend::name`]).
    pub name: String,
    pub kind: StorageKind,
    /// Human-readable endpoint, e.g. `nfs://10.0.0.5:/export/pool`,
    /// `smb://nas/media`, `proxmox:node/local-lvm`. Never contains secrets.
    pub endpoint: String,
    pub capabilities: Vec<Capability>,
}

/// A single share/volume exposed by a backend.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Share {
    /// Stable id within the backend (export path, share name, storage id).
    pub id: String,
    /// Source as it would appear in a mount command / fstab
    /// (`host:/export`, `//server/share`, …).
    pub source: String,
    /// Where it is (or should be) mounted, when known.
    #[serde(default)]
    pub target: Option<String>,
    /// Filesystem / transport type (`nfs`, `nfs4`, `cifs`, `zfs`, `dir`, …).
    pub fstype: String,
    /// Whether the share is currently mounted at `target` (probed, not assumed).
    #[serde(default)]
    pub mounted: bool,
}

/// Result of a mount/unmount operation. `recovered` is set when the backend had
/// to run its stale-handle recovery sequence (lazy unmount + remount) to reach
/// the requested state — surfaced so the reconciler can record self-healing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MountOutcome {
    pub target: String,
    pub mounted: bool,
    #[serde(default)]
    pub recovered: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Capacity/usage snapshot for a volume.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    pub id: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("capability not supported by backend `{0}`: {1:?}")]
    Unsupported(String, Capability),
    #[error("share not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

// ── Backend trait ─────────────────────────────────────────────────────────

/// A storage provider adapter. nfs/smb implement network-share backends;
/// proxmox implements an API-managed disk-storage backend. Default trait
/// methods return [`StorageError::Unsupported`] so a backend only overrides
/// the operations its [`StorageBackend::capabilities`] advertise.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> StorageKind;
    fn capabilities(&self) -> Vec<Capability>;

    /// Provider descriptor for `storage.list` / topology.
    fn provider(&self) -> Provider {
        Provider {
            name: self.name().to_string(),
            kind: self.kind(),
            endpoint: self.endpoint(),
            capabilities: self.capabilities(),
        }
    }

    /// Non-secret endpoint string for display.
    fn endpoint(&self) -> String;

    fn supports(&self, cap: Capability) -> bool {
        self.capabilities().contains(&cap)
    }

    async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
        Err(StorageError::Unsupported(
            self.name().into(),
            Capability::List,
        ))
    }

    async fn mount(&self, _id: &str, _target: &str) -> Result<MountOutcome, StorageError> {
        Err(StorageError::Unsupported(
            self.name().into(),
            Capability::Mount,
        ))
    }

    async fn unmount(&self, _target: &str) -> Result<MountOutcome, StorageError> {
        Err(StorageError::Unsupported(
            self.name().into(),
            Capability::Unmount,
        ))
    }

    async fn usage(&self, _id: &str) -> Result<Usage, StorageError> {
        Err(StorageError::Unsupported(
            self.name().into(),
            Capability::Usage,
        ))
    }

    /// Probe every (optionally `watch`-filtered) mount this backend manages,
    /// self-heal any stale or vanished ones, and report the outcome. `watch` is
    /// an optional allow-list of mountpoints (empty = all); `health_timeout`
    /// bounds each per-mount liveness probe.
    ///
    /// Default is a no-op success so backends that can't self-heal (disk
    /// storage, object stores) need not override it; the empty
    /// [`RecoverOutcome`] reports `no_stale_found = true`.
    async fn recover_stale(
        &self,
        _watch: &[String],
        _health_timeout: std::time::Duration,
    ) -> Result<RecoverOutcome, StorageError> {
        Ok(RecoverOutcome {
            no_stale_found: true,
            ..Default::default()
        })
    }
}

// ── Process-global registry ─────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn StorageBackend>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a storage backend with the process-global registry. Each adapter
/// (nfs, smb, proxmox, …) calls this from its bootstrap once per configured
/// provider. Re-registering the same name replaces the existing entry so a
/// dev rebuild / reconnect doesn't duplicate providers.
pub fn register_backend(backend: Arc<dyn StorageBackend>) {
    let mut g = GLOBAL.write().expect("storage registry poisoned");
    let name = backend.name().to_string();
    if let Some(slot) = g.iter_mut().find(|b| b.name() == name) {
        *slot = backend;
    } else {
        g.push(backend);
    }
}

/// Snapshot of every registered backend. Consumers iterate this rather than
/// naming specific storage kinds.
pub fn backends() -> Vec<Arc<dyn StorageBackend>> {
    GLOBAL.read().expect("storage registry poisoned").clone()
}

/// Look up a single backend by name.
pub fn backend(name: &str) -> Option<Arc<dyn StorageBackend>> {
    GLOBAL
        .read()
        .expect("storage registry poisoned")
        .iter()
        .find(|b| b.name() == name)
        .cloned()
}

/// Descriptor rows for every registered provider — the `storage.list` view.
pub fn providers() -> Vec<Provider> {
    backends().iter().map(|b| b.provider()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeNas {
        name: String,
    }

    #[async_trait]
    impl StorageBackend for FakeNas {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> StorageKind {
            StorageKind::NetworkShare
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::List, Capability::Mount, Capability::Unmount]
        }
        fn endpoint(&self) -> String {
            "nfs://nas/pool".into()
        }
        async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
            Ok(vec![Share {
                id: "pool".into(),
                source: "nas:/export/pool".into(),
                target: Some("/mnt/pool".into()),
                fstype: "nfs4".into(),
                mounted: true,
            }])
        }
    }

    #[tokio::test]
    async fn register_dedupes_by_name_and_lists_providers() {
        register_backend(Arc::new(FakeNas {
            name: "nas-a".into(),
        }));
        register_backend(Arc::new(FakeNas {
            name: "nas-a".into(),
        }));
        assert_eq!(backends().iter().filter(|b| b.name() == "nas-a").count(), 1);
        let p = backend("nas-a").expect("registered");
        assert_eq!(p.kind(), StorageKind::NetworkShare);
        assert!(p.supports(Capability::Mount));
        assert!(!p.supports(Capability::Create));
    }

    #[tokio::test]
    async fn unsupported_capability_errors_without_override() {
        let nas = FakeNas {
            name: "nas-b".into(),
        };
        let err = nas.usage("pool").await.expect_err("usage unsupported");
        assert!(matches!(
            err,
            StorageError::Unsupported(_, Capability::Usage)
        ));
        let shares = nas.list_shares().await.expect("list supported");
        assert_eq!(shares.len(), 1);
    }
}
