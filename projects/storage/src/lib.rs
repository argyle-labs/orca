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

use derive::orca_async;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, RwLock};
use thiserror::Error;

/// Cross-platform kernel-mount-table primitive shared by every network-share
/// backend (nfs, smb, …). Plugins read the live table and classify health
/// through this rather than each parsing `/proc/mounts` themselves.
pub mod mount_table;

/// Generic mount-option mechanics (tokenize / safety-floor / build) shared by
/// every network-share backend. Zero fstype grammar — the plugin supplies the
/// values.
pub mod options;

/// Typed per-mount remount policy (aggression / failover / drain) shared by the
/// convergence engine and the backend plugins.
pub mod remount_policy;

/// Observed replication health — the read side of a replication relationship,
/// plus the provider seam that yields it (mirrors the `StorageBackend` registry).
pub mod replication_status;

pub use mount_table::{
    Health, MountEntry, mount_table, mount_table_of, probe_health, probe_source, probe_source_nfs,
    source_endpoint,
};
pub use options::{
    MountOpt, OptionBuilder, apply_option_floor, option_present, parse_option_string,
};
pub use remount_policy::{
    Drain, DrainMode, Failover, RemountAggression, RemountPolicy, SourceProbe,
};
pub use replication_status::{
    ReplicationStatus, ReplicationStatusProvider, deregister_status_provider,
    register_status_provider, resolve as resolve_replication_status, status_providers,
};

// ── Mount contract (Phase 1) ──────────────────────────────────────────────────
//
// The typed mount lifecycle contract. orca core owns the declarative mount store
// (`managed_mounts`) and the autofs applier; these types let a backend own the
// grammar of its own mount options (nfs vers/timeo/hard, smb creds) and pick how
// its mounts are realized (kernel mount vs a userspace helper process), instead
// of core treating options as an opaque comma-string. All types are plain serde
// so they cross the plugin JSON/FFI boundary unchanged.

/// How a backend's mounts are realized on the host. Network shares (nfs, smb) are
/// kernel mounts driven through autofs; a future object-store backend runs a
/// userspace helper process (e.g. a FUSE/gateway daemon) instead. The default is
/// [`MountStyle::KernelMount`] so every existing backend keeps today's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MountStyle {
    /// Realized as a kernel mount (fstab/autofs). The path every network-share
    /// backend takes today.
    #[default]
    KernelMount,
    /// Realized by a long-lived userspace process the backend supervises rather
    /// than a kernel mount entry.
    UserspaceProcess,
}

/// A reference to a credential the secrets domain resolves — a `SecretRef` string
/// such as `onepassword://…`, `bitwarden://…`, or a native secret id. Modeled as
/// a newtype (not a bare `String`) so the mount contract is explicit about which
/// fields are credential references; it (de)serializes transparently as its inner
/// string, matching how `managed_mounts.credential` is already persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SecretRef(pub String);

/// Directory holding root-owned `0600` secret files a backend needs materialized
/// on the host before a mount (an SMB `credentials=` file, a Kerberos keytab, …).
/// One file per mount that declares such a secret; the mount references it by path
/// so the secret never sits inline in the world-readable autofs map. Under
/// `/etc/orca` (orca-owned config root) so the privileged applier's allowlist can
/// scope writes to exactly this subtree. Generic core primitive: core knows the
/// directory + path convention, never the file's grammar (which the owning plugin
/// renders). Backend-agnostic — not SMB-specific.
pub const SECRET_FILE_DIR: &str = "/etc/orca/secret-files";

/// Legacy secret-file directory (the SMB-named `/etc/orca/smb-creds`) used before
/// the seam was genericized. The privileged applier removes it wholesale once, as
/// a one-time filesystem migration, so no root-owned secret files are orphaned on
/// disk. Safe to delete this const (and its cleanup) once the fleet has converged
/// onto [`SECRET_FILE_DIR`].
pub const LEGACY_SECRET_FILE_DIR: &str = "/etc/orca/smb-creds";

/// The canonical, collision-free, traversal-proof secret-file path for a mount
/// `target`. The filename is a slug of the absolute target: every non
/// `[A-Za-z0-9]` byte becomes `_`, leading `_` trimmed, so `/mnt/media` →
/// `mnt_media.secret`. Because the slug contains no `/` or `.` runs, the result
/// can never escape [`SECRET_FILE_DIR`] — the property the privileged allowlist
/// relies on to reject a traversal or out-of-subtree path.
///
/// Deterministic: the same target always yields the same path, so re-applying a
/// mount overwrites its own secret-file and teardown can compute exactly which
/// file to remove. Shared by the daemon-side writer and the root-side allowlist
/// so the two never disagree on where a mount's secret-file lives.
pub fn secret_file_path(target: &str) -> String {
    let slug: String = target
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '_'
            }
        })
        .collect();
    let slug = slug.trim_matches('_');
    let slug = if slug.is_empty() { "root" } else { slug };
    format!("{SECRET_FILE_DIR}/{slug}.secret")
}

/// Is `path` a legal secret-file path — inside [`SECRET_FILE_DIR`], a single
/// `<slug>.secret` component, with no traversal? The privileged allowlist calls
/// this to admit a secret-file write without opening the door to arbitrary paths.
/// A path is legal iff it round-trips: recomputing [`secret_file_path`] from the
/// slug it carries reproduces the path exactly, which forecloses `..`, nested
/// components, and any name a target could not have produced.
pub fn is_valid_secret_file_path(path: &str) -> bool {
    let Some(name) = path.strip_prefix(&format!("{SECRET_FILE_DIR}/")) else {
        return false;
    };
    // Exactly one path component ending in `.secret`, no separators or traversal.
    if name.contains('/') || name.contains("..") {
        return false;
    }
    let Some(slug) = name.strip_suffix(".secret") else {
        return false;
    };
    !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A root-owned secret file a backend needs materialized on the host before its
/// mount runs (an SMB `credentials=<path>` file, a keytab, …). The generic
/// secret-file seam: the owning backend resolves its own [`SecretRef`] and renders
/// `contents`; core writes the bytes to `path` (mode `0600`, path validated via
/// [`is_valid_secret_file_path`]) before mounting and reaps it on teardown, never
/// knowing the file's grammar. `path` must be a legal secret-file path under
/// [`SECRET_FILE_DIR`] — typically [`secret_file_path`] of the mount target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretFile {
    pub path: String,
    pub contents: String,
}

/// The validated option set core carries between a backend's `validate_spec` and
/// `render_options`. Core is fstype-agnostic: it holds only the opaque, already
/// comma-joined option string a backend produced — every backend owns the grammar
/// of its own options entirely inside its plugin (parsing at declare time,
/// rendering — including any safety floor — locally). Core neither parses nor
/// interprets these bytes; it round-trips them verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "fs")]
pub enum OptionSet {
    /// The raw comma string a backend rendered (or the declared string verbatim
    /// for an un-migrated backend). The only form core knows.
    Raw {
        #[serde(default)]
        options: Option<String>,
    },
}

/// A declarative mount as the core store holds it, in typed form — the input to
/// [`StorageBackend::validate_spec`]. Core builds this from a `managed_mounts` row
/// (whose `options`/`kind`/`credential` are still strings) and hands it to the
/// owning backend to validate. Plain serde so it crosses the plugin boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MountSpec {
    /// Registered backend that owns this mount (`nfs`, `smb`, …).
    pub backend: String,
    /// Absolute mountpoint / target path.
    pub target: String,
    /// Filesystem / transport type (`nfs4`, `cifs`, …).
    pub fstype: String,
    /// Primary source as the backend expects it (`host:/export`, `//server/share`).
    pub source: String,
    /// Ordered failover sources (secondaries), tried after `source`.
    #[serde(default)]
    pub failover_sources: Vec<String>,
    /// Raw option string as declared in the store (`vers=4.2,hard,_netdev`).
    #[serde(default)]
    pub options: Option<String>,
    /// Credential reference the secrets domain resolves. Never a plaintext secret.
    #[serde(default)]
    pub credential: Option<SecretRef>,
    /// Typed remount policy (the backend carries it through; core owns the engine).
    #[serde(default)]
    pub remount_policy: Option<RemountPolicy>,
    pub enabled: bool,
}

/// A [`MountSpec`] a backend has validated and normalized: the raw option string
/// parsed into a typed [`OptionSet`] the backend guarantees it can render. Carries
/// the same identity fields as the spec so downstream consumers (the autofs
/// renderer) need only the normalized form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NormalizedSpec {
    pub backend: String,
    pub target: String,
    pub fstype: String,
    pub source: String,
    #[serde(default)]
    pub failover_sources: Vec<String>,
    /// The validated, typed option set.
    pub options: OptionSet,
    #[serde(default)]
    pub credential: Option<SecretRef>,
    /// A root-owned secret-file the backend needs materialized before mounting
    /// (inline-SMB creds). The backend resolves its own [`SecretRef`] and renders
    /// `contents` here; core writes it 0600 and reaps it, never parsing it. `None`
    /// for NFS and file/guest-SMB.
    #[serde(default)]
    pub secret_file: Option<SecretFile>,
    #[serde(default)]
    pub remount_policy: Option<RemountPolicy>,
    pub enabled: bool,
}

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
    /// Enumerate the exports this host *serves* (NFS `/etc/exports`, SMB shares),
    /// as distinct from what it mounts.
    Exports,
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

/// A single export this host *serves* to the network — one NFS `/etc/exports`
/// line or one SMB share definition. The read-side inverse of a [`Share`]: a
/// [`Share`] is something a host can mount, an [`ExportEntry`] is something a
/// host publishes for others to mount. Modeled from the fields an export
/// definition carries; the owning backend fills what it can read and leaves the
/// rest empty/`None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExportEntry {
    /// Exported path on this host (`/export/pool`, the SMB share's `path=`).
    pub path: String,
    /// Clients/subnets allowed to mount this export (`10.0.0.0/24`, `*`,
    /// hostnames). Empty when the backend can't enumerate them.
    #[serde(default)]
    pub allowed_clients: Vec<String>,
    /// Export options as the server declares them (`rw,sync,no_subtree_check`,
    /// SMB share flags). Empty when none/unknown.
    #[serde(default)]
    pub options: Vec<String>,
    /// NFS filesystem id (`fsid=`), when the backend knows it.
    #[serde(default)]
    pub fsid: Option<String>,
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
#[orca_async]
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

    /// How this backend's mounts are realized on the host. Network shares are
    /// kernel mounts (the default); an object-store backend overrides this to
    /// [`MountStyle::UserspaceProcess`] so core drives its helper path instead of
    /// the autofs map.
    fn mount_style(&self) -> MountStyle {
        MountStyle::KernelMount
    }

    /// The kernel filesystem-type strings this backend's mounts appear as in the
    /// mount table (`nfs4`/`nfs` for nfs, `cifs`/`smbfs` for smb). Core unions
    /// these across every registered backend to learn which mount-table entries
    /// are network shares it elects/reads sources for — so no fstype literal lives
    /// in core. Default empty: a backend with no kernel-mount presence (disk,
    /// object) contributes nothing.
    fn net_fstypes(&self) -> Vec<String> {
        Vec::new()
    }

    /// The default TCP port core probes to test transport liveness of a source
    /// this backend owns (nfs → `2049`, smb → `445`). The port is fstype grammar
    /// the owning plugin holds, not core: core parses a source into its host
    /// generically, then asks the backend that owns the fstype for the port to
    /// probe. Default `None`: a backend with no network transport to probe (disk,
    /// object) contributes no port and its sources are not TCP-probed.
    fn default_source_port(&self) -> Option<u16> {
        None
    }

    /// Validate + normalize a declarative mount spec, turning its raw option
    /// string into the typed [`OptionSet`] this backend guarantees it can render.
    /// A backend that owns an option grammar (nfs vers/timeo/hard, smb creds)
    /// overrides this to reject malformed options at declare time rather than at
    /// mount time.
    ///
    /// The default is an identity normalization: it carries the spec's fields
    /// through and wraps the raw option string in [`OptionSet::Raw`], so an
    /// un-migrated backend (and the JSON proxy) validates every spec exactly as
    /// core handled it before this method existed.
    async fn validate_spec(&self, spec: &MountSpec) -> Result<NormalizedSpec, StorageError> {
        Ok(NormalizedSpec {
            backend: spec.backend.clone(),
            target: spec.target.clone(),
            fstype: spec.fstype.clone(),
            source: spec.source.clone(),
            failover_sources: spec.failover_sources.clone(),
            options: OptionSet::Raw {
                options: spec.options.clone(),
            },
            credential: spec.credential.clone(),
            secret_file: None,
            remount_policy: spec.remount_policy.clone(),
            enabled: spec.enabled,
        })
    }

    /// Render a normalized spec's options back into the comma-joined option string
    /// the kernel mount / mount helper consumes. Core applies its own fstab-only
    /// filter (stripping `_netdev`/`nofail`) to the result before writing the
    /// autofs map, so a backend renders the full option set here and need not know
    /// about autofs.
    ///
    /// The default renders [`OptionSet::Raw`] as the original string verbatim
    /// (and any typed variant on a best-effort basis), preserving the exact bytes
    /// core produced before backends owned rendering.
    fn render_options(&self, spec: &NormalizedSpec) -> String {
        render_option_set(&spec.options)
    }

    async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
        Err(StorageError::Unsupported(
            self.name().into(),
            Capability::List,
        ))
    }

    /// Enumerate the exports this host *serves* — the read-side inverse of
    /// [`list_shares`](StorageBackend::list_shares). The default reports
    /// [`Capability::Exports`] as unsupported so the `storage.exports` aggregator
    /// skips a backend that doesn't advertise it; the actual readers (nfs parsing
    /// `/etc/exports` / `showmount -e`, unraid reading share config) live in their
    /// owning plugins.
    async fn list_exports(&self) -> Result<Vec<ExportEntry>, StorageError> {
        Err(StorageError::Unsupported(
            self.name().into(),
            Capability::Exports,
        ))
    }

    /// Bring share `id` up at `target`. Vestigial for kernel-mount backends —
    /// autofs owns their mount mechanics, so nfs/smb leave this at the default.
    /// It is retained as the entry point a [`MountStyle::UserspaceProcess`]
    /// backend will drive its helper process through (an object-store gateway,
    /// realized later); kept now so that contract has a stable home.
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

/// Render an [`OptionSet`] into the comma-joined mount-option string. Core holds
/// only the opaque [`OptionSet::Raw`] form, so this reproduces the declared string
/// verbatim — byte-identical to what a backend rendered (or, for an un-migrated
/// backend, the declared string). The canonical renderer the default
/// [`StorageBackend::render_options`] delegates to.
pub fn render_option_set(set: &OptionSet) -> String {
    match set {
        OptionSet::Raw { options } => options.clone().unwrap_or_default(),
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

/// Deregister the backend named `name`, if present. The removal path the
/// reload/unload flow needs: a plugin's domain-registration must be reversible
/// so unloading a plugin drops its providers from the registry rather than
/// leaving stale rows pointing at an invoke thunk whose plugin is gone.
/// Returns `true` if a backend was removed.
pub fn deregister_backend(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("storage registry poisoned");
    let before = g.len();
    g.retain(|b| b.name() != name);
    before != g.len()
}

/// The synchronous invoke thunk a loaded plugin's domain backend is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. The loader
/// supplies a closure that marshals `op` into a `"{invoke_prefix}.{op}"` tool
/// call over the subprocess wire. Kept as a plain `Fn` of strings so
/// the `storage` crate stays free of any dependency on the loader crates
/// (no cycle): the loader owns the transport, storage owns the domain shape.
///
/// Host-side (in-process) only: the thunk drives a *loaded plugin* over the
/// subprocess wire — a daemon/host concern. A thin subprocess plugin links no loader
/// path and no tokio, so the whole proxy surface is gated out on thin,
/// consistent with `http`/`db` being capabilities rather than always-linked.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> Result<String, StorageError> + Send + Sync + 'static>;

/// Build and register a [`StorageBackend`] from a plugin's backend descriptor
/// plus an [`InvokeThunk`]. The loader calls this from its domain dispatch
/// table (storage being the first entry); it parses `kind`/`capabilities` into
/// the domain enums and wires every advertised operation back through `invoke`.
///
/// `kind` / `capabilities` are the raw strings from the plugin's `BackendDef`;
/// unknown values are rejected so a typo surfaces at load, not at first use.
/// Registration replaces any existing backend of the same name (idempotent
/// reload), matching [`register_backend`]'s semantics.
#[cfg(feature = "in-process")]
pub fn register_from_def(
    name: String,
    kind: &str,
    endpoint: String,
    capabilities: &[String],
    invoke: InvokeThunk,
) -> Result<(), StorageError> {
    register_from_def_styled(name, kind, endpoint, capabilities, "", &[], None, invoke)
}

/// [`register_from_def`] carrying the backend's mount-style axis from its
/// `BackendDef`. `mount_style` is the raw wire string (`""`/`"kernel_mount"` =
/// kernel, `"userspace_process"` = helper); an unknown value is rejected at load.
/// The zero-axis [`register_from_def`] defaults it to kernel so a caller that
/// doesn't pass the axis keeps its exact prior behavior.
// A wire-shaped constructor: each parameter is a distinct axis parsed off the
// plugin's `BackendDef`, so grouping them into a struct would just re-flatten at
// both call sites for no clarity gain.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "in-process")]
pub fn register_from_def_styled(
    name: String,
    kind: &str,
    endpoint: String,
    capabilities: &[String],
    mount_style: &str,
    net_fstypes: &[String],
    default_source_port: Option<u16>,
    invoke: InvokeThunk,
) -> Result<(), StorageError> {
    let kind = parse_kind(kind)?;
    let mount_style = parse_mount_style(mount_style)?;
    let capabilities = capabilities
        .iter()
        .map(|c| parse_capability(c))
        .collect::<Result<Vec<_>, _>>()?;
    register_backend(Arc::new(StorageProxy {
        name,
        kind,
        endpoint,
        capabilities,
        mount_style,
        net_fstypes: net_fstypes.to_vec(),
        default_source_port,
        invoke,
    }));
    Ok(())
}

#[cfg(feature = "in-process")]
fn parse_mount_style(s: &str) -> Result<MountStyle, StorageError> {
    match s {
        "" | "kernel_mount" => Ok(MountStyle::KernelMount),
        "userspace_process" => Ok(MountStyle::UserspaceProcess),
        other => Err(StorageError::Other(format!(
            "unknown storage mount_style `{other}`"
        ))),
    }
}

#[cfg(feature = "in-process")]
fn parse_kind(s: &str) -> Result<StorageKind, StorageError> {
    match s {
        "network_share" => Ok(StorageKind::NetworkShare),
        "disk_storage" => Ok(StorageKind::DiskStorage),
        "object" => Ok(StorageKind::Object),
        other => Err(StorageError::Other(format!(
            "unknown storage kind `{other}`"
        ))),
    }
}

#[cfg(feature = "in-process")]
fn parse_capability(s: &str) -> Result<Capability, StorageError> {
    match s {
        "list" => Ok(Capability::List),
        "exports" => Ok(Capability::Exports),
        "mount" => Ok(Capability::Mount),
        "unmount" => Ok(Capability::Unmount),
        "usage" => Ok(Capability::Usage),
        "create" => Ok(Capability::Create),
        "remove" => Ok(Capability::Remove),
        "recover_stale" => Ok(Capability::RecoverStale),
        other => Err(StorageError::Other(format!(
            "unknown storage capability `{other}`"
        ))),
    }
}

/// A [`StorageBackend`] backed by a subprocess plugin reached over the JSON-proxy
/// wire. Each async trait method serializes its args to JSON, offloads
/// the synchronous [`InvokeThunk`] onto `spawn_blocking` (so a slow/wedged
/// plugin never blocks the async runtime), and deserializes the JSON result.
#[cfg(feature = "in-process")]
struct StorageProxy {
    name: String,
    kind: StorageKind,
    endpoint: String,
    capabilities: Vec<Capability>,
    mount_style: MountStyle,
    net_fstypes: Vec<String>,
    default_source_port: Option<u16>,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl StorageProxy {
    /// Run one proxied op on the blocking pool and deserialize its JSON result.
    /// `op` is the bare operation name (the loader's thunk prepends the
    /// plugin's invoke prefix); `args` is the op's typed args object.
    async fn call<A, R>(&self, op: &'static str, args: A) -> Result<R, StorageError>
    where
        A: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| StorageError::Other(format!("encode `{op}` args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
            .await
            .map_err(|e| StorageError::Transport(format!("`{op}` proxy task failed: {e}")))??;
        serde_json::from_str(&out)
            .map_err(|e| StorageError::Other(format!("decode `{op}` result: {e}")))
    }
}

#[cfg(feature = "in-process")]
#[orca_async]
impl StorageBackend for StorageProxy {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> StorageKind {
        self.kind
    }
    fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.clone()
    }
    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn mount_style(&self) -> MountStyle {
        self.mount_style
    }

    fn net_fstypes(&self) -> Vec<String> {
        self.net_fstypes.clone()
    }

    fn default_source_port(&self) -> Option<u16> {
        self.default_source_port
    }

    /// Proxied validation: the plugin owns its option grammar, so validation is
    /// routed over the wire to it. The plugin returns the typed [`NormalizedSpec`].
    async fn validate_spec(&self, spec: &MountSpec) -> Result<NormalizedSpec, StorageError> {
        self.call("validate_spec", ValidateSpecArgs { spec: spec.clone() })
            .await
    }

    /// Rendered locally from the already-validated typed [`OptionSet`]: rendering
    /// is a deterministic function of the normalized spec the plugin produced, so
    /// no second round-trip is needed (and this method is sync — it cannot await).
    fn render_options(&self, spec: &NormalizedSpec) -> String {
        render_option_set(&spec.options)
    }

    async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
        self.call("list_shares", NoArgs {}).await
    }

    async fn list_exports(&self) -> Result<Vec<ExportEntry>, StorageError> {
        self.call("list_exports", NoArgs {}).await
    }

    async fn mount(&self, id: &str, target: &str) -> Result<MountOutcome, StorageError> {
        self.call(
            "mount",
            MountArgs {
                id: id.to_string(),
                target: target.to_string(),
            },
        )
        .await
    }

    async fn unmount(&self, target: &str) -> Result<MountOutcome, StorageError> {
        self.call(
            "unmount",
            UnmountArgs {
                target: target.to_string(),
            },
        )
        .await
    }

    async fn usage(&self, id: &str) -> Result<Usage, StorageError> {
        self.call("usage", IdArg { id: id.to_string() }).await
    }

    async fn recover_stale(
        &self,
        watch: &[String],
        health_timeout: std::time::Duration,
    ) -> Result<RecoverOutcome, StorageError> {
        self.call(
            "recover_stale",
            RecoverArgs {
                watch: watch.to_vec(),
                health_timeout_secs: health_timeout.as_secs_f64(),
            },
        )
        .await
    }
}

// ── Proxy wire-args ───────────────────────────────────────────────────────
// Typed args objects each proxied op serializes across the FFI invoke boundary.
// Defined (not `json!`'d) so the wire contract is explicit and a plugin's
// `invoke` arm deserializes against the same shape — no opaque `Value`.

// Serialize-only: only the host-side proxy encodes it; `dispatch_op` decodes the
// other arg structs. Gated with the proxy so the thin profile stays warning-free.
#[cfg(feature = "in-process")]
#[derive(Serialize)]
struct NoArgs {}

#[derive(Serialize, Deserialize)]
struct MountArgs {
    id: String,
    target: String,
}

#[derive(Serialize, Deserialize)]
struct UnmountArgs {
    target: String,
}

#[derive(Serialize, Deserialize)]
struct IdArg {
    id: String,
}

#[derive(Serialize, Deserialize)]
struct RecoverArgs {
    watch: Vec<String>,
    health_timeout_secs: f64,
}

#[derive(Serialize, Deserialize)]
struct ValidateSpecArgs {
    spec: MountSpec,
}

/// Plugin-side inverse of [`StorageProxy`]: decode a proxied op's JSON args and
/// route it to an in-process [`StorageBackend`], returning the op's
/// JSON-encoded result (or an error string).
///
/// Both halves of the storage FFI boundary live here so the wire contract has a
/// single source of truth: `StorageProxy` (orca side) encodes `op` + args into
/// `"{invoke_prefix}.{op}"` calls; this (plugin side) decodes them back against
/// the *same* wire-arg structs and dispatches to the backend. A backend
/// plugin's `invoke` is therefore one call to this function — never a
/// hand-copied per-op `match` that drifts from the proxy. `op` is the bare
/// operation name (the loader's thunk strips the invoke prefix first).
pub async fn dispatch_op(
    backend: &dyn StorageBackend,
    op: &str,
    args_json: &str,
) -> Result<String, String> {
    fn enc<T: Serialize>(value: &T) -> Result<String, String> {
        serde_json::to_string(value).map_err(|e| format!("failed to encode result: {e}"))
    }
    fn dec<T: serde::de::DeserializeOwned>(op: &str, args_json: &str) -> Result<T, String> {
        serde_json::from_str(args_json).map_err(|e| format!("invalid `{op}` args: {e}"))
    }

    match op {
        "list_shares" => enc(&backend.list_shares().await.map_err(|e| e.to_string())?),
        "list_exports" => enc(&backend.list_exports().await.map_err(|e| e.to_string())?),
        "mount" => {
            let a: MountArgs = dec(op, args_json)?;
            enc(&backend
                .mount(&a.id, &a.target)
                .await
                .map_err(|e| e.to_string())?)
        }
        "unmount" => {
            let a: UnmountArgs = dec(op, args_json)?;
            enc(&backend
                .unmount(&a.target)
                .await
                .map_err(|e| e.to_string())?)
        }
        "usage" => {
            let a: IdArg = dec(op, args_json)?;
            enc(&backend.usage(&a.id).await.map_err(|e| e.to_string())?)
        }
        "recover_stale" => {
            let a: RecoverArgs = dec(op, args_json)?;
            let timeout = std::time::Duration::from_secs_f64(a.health_timeout_secs);
            enc(&backend
                .recover_stale(&a.watch, timeout)
                .await
                .map_err(|e| e.to_string())?)
        }
        "validate_spec" => {
            let a: ValidateSpecArgs = dec(op, args_json)?;
            enc(&backend
                .validate_spec(&a.spec)
                .await
                .map_err(|e| e.to_string())?)
        }
        other => Err(format!("backend has no operation '{other}'")),
    }
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

/// The TCP port to probe for a source of filesystem type `fstype`, resolved from
/// the registered backend that owns it. Core holds no port literal: it finds the
/// backend whose [`StorageBackend::net_fstypes`] contains `fstype` and returns
/// that backend's [`StorageBackend::default_source_port`] (nfs → `2049`, smb →
/// `445`). `None` when no registered backend owns the fstype or the owner
/// declares no probe port — the source is then not TCP-probed. The seam that
/// keeps the port number in the owning plugin, not in [`source_endpoint`].
pub fn source_port_for_fstype(fstype: &str) -> Option<u16> {
    backends()
        .iter()
        .find(|b| b.net_fstypes().iter().any(|t| t == fstype))
        .and_then(|b| b.default_source_port())
}

// The suite drives async via `#[tokio::test]` and exercises the host-side
// `register_from_def` proxy, so it is owned by the `in-process` profile (the one
// that links tokio). `cargo test -p storage` uses the default (in-process)
// profile; a thin `--no-default-features` build compiles with no tests rather
// than dragging tokio in as a dev-dep on the reactor-free profile.
#[cfg(all(test, feature = "in-process"))]
mod tests {
    use super::*;

    struct FakeNas {
        name: String,
    }

    #[orca_async]
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
        async fn unmount(&self, target: &str) -> Result<MountOutcome, StorageError> {
            Ok(MountOutcome {
                target: target.to_string(),
                mounted: false,
                recovered: false,
                detail: None,
            })
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
    async fn dispatch_op_routes_each_op_to_the_backend() {
        let nas = FakeNas {
            name: "nas-d".into(),
        };
        // list_shares: NoArgs in, JSON Vec<Share> out.
        let out = dispatch_op(&nas, "list_shares", "{}")
            .await
            .expect("list_shares dispatch");
        let shares: Vec<Share> = serde_json::from_str(&out).expect("decode shares");
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].id, "pool");

        // unmount: typed UnmountArgs decoded against the proxy's wire struct.
        let out = dispatch_op(&nas, "unmount", r#"{"target":"/mnt/pool"}"#)
            .await
            .expect("unmount dispatch");
        let outcome: MountOutcome = serde_json::from_str(&out).expect("decode outcome");
        assert_eq!(outcome.target, "/mnt/pool");
    }

    #[tokio::test]
    async fn dispatch_op_surfaces_unsupported_and_unknown() {
        let nas = FakeNas {
            name: "nas-e".into(),
        };
        // `usage` is a real op but unsupported by this backend → error string.
        let e = dispatch_op(&nas, "usage", r#"{"id":"pool"}"#)
            .await
            .expect_err("usage unsupported");
        assert!(e.contains("not supported"), "got: {e}");

        // A name that is not a storage op at all.
        let e = dispatch_op(&nas, "frobnicate", "{}")
            .await
            .expect_err("unknown op");
        assert!(e.contains("no operation 'frobnicate'"), "got: {e}");

        // Malformed args for a known op → decode error, not a panic.
        let e = dispatch_op(&nas, "unmount", "not json")
            .await
            .expect_err("bad args");
        assert!(e.contains("invalid `unmount` args"), "got: {e}");
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

    #[tokio::test]
    async fn register_from_def_proxies_ops_and_deregisters() {
        // Thunk standing in for the FFI invoke boundary: it answers the two ops
        // the proxy calls, (de)serializing through the same typed domain structs
        // the real boundary uses — no opaque `Value`.
        let thunk: InvokeThunk = Arc::new(|op: &str, args_json: String| match op {
            "list_shares" => {
                let shares = vec![Share {
                    id: "pool".into(),
                    source: "nas:/export/pool".into(),
                    target: Some("/mnt/pool".into()),
                    fstype: "nfs4".into(),
                    mounted: true,
                }];
                Ok(serde_json::to_string(&shares).unwrap())
            }
            "unmount" => {
                let a: UnmountArgs = serde_json::from_str(&args_json).unwrap();
                let out = MountOutcome {
                    target: a.target,
                    mounted: false,
                    recovered: true,
                    detail: None,
                };
                Ok(serde_json::to_string(&out).unwrap())
            }
            other => Err(StorageError::Other(format!("unexpected op {other}"))),
        });

        register_from_def(
            "proxy-nas".into(),
            "network_share",
            "nfs://proxy/pool".into(),
            &["list".into(), "unmount".into()],
            thunk,
        )
        .expect("def registers");

        let b = backend("proxy-nas").expect("registered");
        assert_eq!(b.kind(), StorageKind::NetworkShare);
        assert!(b.supports(Capability::List) && b.supports(Capability::Unmount));

        let shares = b.list_shares().await.expect("proxied list_shares");
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].id, "pool");

        let out = b.unmount("/mnt/pool").await.expect("proxied unmount");
        assert_eq!(out.target, "/mnt/pool");
        assert!(out.recovered && !out.mounted);

        assert!(deregister_backend("proxy-nas"));
        assert!(backend("proxy-nas").is_none());
        assert!(!deregister_backend("proxy-nas"));
    }

    #[test]
    fn register_from_def_rejects_unknown_kind_and_capability() {
        let thunk: InvokeThunk = Arc::new(|_, _| Ok("null".into()));
        assert!(register_from_def("x".into(), "nope", "e".into(), &[], thunk.clone()).is_err());
        assert!(
            register_from_def("x".into(), "object", "e".into(), &["fly".into()], thunk).is_err()
        );
    }

    // ── Mount contract (Phase 1) ──────────────────────────────────────────

    fn nfs_spec(options: Option<&str>) -> MountSpec {
        MountSpec {
            backend: "nfs".into(),
            target: "/mnt/pool".into(),
            fstype: "nfs4".into(),
            source: "nas:/export/pool".into(),
            failover_sources: vec![],
            options: options.map(str::to_string),
            credential: None,
            remount_policy: None,
            enabled: true,
        }
    }

    #[test]
    fn default_mount_style_is_kernel_mount() {
        let nas = FakeNas { name: "s".into() };
        assert_eq!(nas.mount_style(), MountStyle::KernelMount);
    }

    #[tokio::test]
    async fn default_validate_spec_is_identity_raw_and_renders_verbatim() {
        // A backend that hasn't migrated must normalize to Raw and render the
        // declared option string byte-for-byte — the backward-compat guarantee.
        let nas = FakeNas { name: "s".into() };
        let spec = nfs_spec(Some("vers=4.2,hard,_netdev"));
        let normalized = nas.validate_spec(&spec).await.expect("identity validate");
        assert_eq!(
            normalized.options,
            OptionSet::Raw {
                options: Some("vers=4.2,hard,_netdev".into())
            }
        );
        assert_eq!(nas.render_options(&normalized), "vers=4.2,hard,_netdev");
    }

    #[tokio::test]
    async fn default_validate_spec_handles_no_options() {
        let nas = FakeNas { name: "s".into() };
        let normalized = nas.validate_spec(&nfs_spec(None)).await.expect("validate");
        assert_eq!(nas.render_options(&normalized), "");
    }

    #[test]
    fn render_option_set_raw_reproduces_declared_string_verbatim() {
        let set = OptionSet::Raw {
            options: Some("vers=4.2,hard,nconnect=4".into()),
        };
        assert_eq!(render_option_set(&set), "vers=4.2,hard,nconnect=4");
        assert_eq!(render_option_set(&OptionSet::Raw { options: None }), "");
    }

    // ── secret-file path convention + validation ──────────────────────────

    #[test]
    fn secret_file_path_slugs_target_under_secret_dir() {
        assert_eq!(
            secret_file_path("/mnt/media"),
            "/etc/orca/secret-files/mnt_media.secret"
        );
        assert_eq!(
            secret_file_path("/mnt/pool/data"),
            "/etc/orca/secret-files/mnt_pool_data.secret"
        );
        // dots and dashes collapse to `_`; result stays a single component.
        assert_eq!(
            secret_file_path("/mnt/a.b-c"),
            "/etc/orca/secret-files/mnt_a_b_c.secret"
        );
    }

    #[test]
    fn secret_file_path_is_deterministic() {
        assert_eq!(secret_file_path("/mnt/x"), secret_file_path("/mnt/x"));
    }

    #[test]
    fn secret_file_path_can_never_escape_the_secret_dir() {
        // A pathological target with traversal bytes still produces a single
        // slugged component inside the secret dir — the `..` becomes `__`.
        let p = secret_file_path("/../../etc/shadow");
        assert!(p.starts_with(&format!("{SECRET_FILE_DIR}/")));
        assert!(!p.contains(".."));
        assert!(is_valid_secret_file_path(&p));
    }

    #[test]
    fn is_valid_secret_file_path_accepts_generated_paths() {
        for t in ["/mnt/media", "/mnt/pool/data", "/srv/share1"] {
            assert!(is_valid_secret_file_path(&secret_file_path(t)), "{t}");
        }
    }

    #[test]
    fn is_valid_secret_file_path_rejects_traversal_and_out_of_scope() {
        assert!(!is_valid_secret_file_path("/etc/passwd"));
        assert!(!is_valid_secret_file_path("/etc/auto.master"));
        assert!(!is_valid_secret_file_path(
            "/etc/orca/secret-files/../../shadow"
        ));
        assert!(!is_valid_secret_file_path(
            "/etc/orca/secret-files/sub/dir.secret"
        ));
        assert!(!is_valid_secret_file_path("/etc/orca/secret-files/.secret"));
        assert!(!is_valid_secret_file_path("/etc/orca/secret-files/x.txt"));
        assert!(!is_valid_secret_file_path(
            "/etc/orca/secret-filesX/x.secret"
        ));
    }

    #[test]
    fn secret_ref_serializes_transparently_as_its_inner_string() {
        assert_eq!(
            serde_json::to_string(&SecretRef("bitwarden://x".into())).unwrap(),
            "\"bitwarden://x\""
        );
    }

    #[test]
    fn mount_spec_and_normalized_spec_round_trip_json() {
        let spec = nfs_spec(Some("ro"));
        let s = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<MountSpec>(&s).unwrap(), spec);

        let normalized = NormalizedSpec {
            backend: "nfs".into(),
            target: "/mnt/pool".into(),
            fstype: "nfs4".into(),
            source: "nas:/e".into(),
            failover_sources: vec!["nas2:/e".into()],
            options: OptionSet::Raw {
                options: Some("ro".into()),
            },
            credential: Some(SecretRef("s".into())),
            secret_file: None,
            remount_policy: None,
            enabled: true,
        };
        let s = serde_json::to_string(&normalized).unwrap();
        assert_eq!(
            serde_json::from_str::<NormalizedSpec>(&s).unwrap(),
            normalized
        );
    }

    #[tokio::test]
    async fn dispatch_op_routes_validate_spec_and_defaults_to_raw() {
        let nas = FakeNas { name: "s".into() };
        let args = serde_json::to_string(&ValidateSpecArgs {
            spec: nfs_spec(Some("vers=4.2,hard")),
        })
        .unwrap();
        let out = dispatch_op(&nas, "validate_spec", &args)
            .await
            .expect("validate_spec dispatch");
        let normalized: NormalizedSpec = serde_json::from_str(&out).expect("decode normalized");
        assert_eq!(
            normalized.options,
            OptionSet::Raw {
                options: Some("vers=4.2,hard".into())
            }
        );
    }

    #[test]
    fn parse_mount_style_maps_wire_strings_and_rejects_garbage() {
        assert_eq!(parse_mount_style("").unwrap(), MountStyle::KernelMount);
        assert_eq!(
            parse_mount_style("kernel_mount").unwrap(),
            MountStyle::KernelMount
        );
        assert_eq!(
            parse_mount_style("userspace_process").unwrap(),
            MountStyle::UserspaceProcess
        );
        assert!(parse_mount_style("nope").is_err());
    }

    #[test]
    fn register_from_def_styled_rejects_unknown_mount_style() {
        let thunk: InvokeThunk = Arc::new(|_, _| Ok("null".into()));
        assert!(
            register_from_def_styled(
                "m".into(),
                "network_share",
                "e".into(),
                &[],
                "weird",
                &[],
                None,
                thunk
            )
            .is_err()
        );
    }

    #[test]
    fn source_port_for_fstype_resolves_from_the_owning_backend() {
        // A proxy backend that owns `nfs4`/`nfs` and declares port 2049 — the
        // fstype→port grammar lives on the backend, not in core.
        let thunk: InvokeThunk = Arc::new(|_, _| Ok("null".into()));
        register_from_def_styled(
            "port-nfs".into(),
            "network_share",
            "nfs://x".into(),
            &[],
            "kernel_mount",
            &["nfs4".into(), "nfs".into()],
            Some(2049),
            thunk,
        )
        .expect("registers");

        assert_eq!(source_port_for_fstype("nfs4"), Some(2049));
        assert_eq!(source_port_for_fstype("nfs"), Some(2049));
        // An fstype no registered backend owns has no probe port.
        assert_eq!(source_port_for_fstype("ext4"), None);

        // And `source_endpoint` composes the generic host parse with the
        // backend-resolved port.
        assert_eq!(
            mount_table::source_endpoint("primary:/srv/pool", "nfs4"),
            Some(("primary".to_string(), 2049))
        );

        deregister_backend("port-nfs");
    }
}
