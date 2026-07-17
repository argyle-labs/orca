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

pub use mount_table::{
    Health, MountEntry, mount_table, mount_table_of, probe_health, probe_source, source_endpoint,
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

/// SMB/CIFS credential source. A backend validates that exactly one coherent form
/// is supplied and renders it into the mount's option string / helper invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SmbCredentials {
    /// A `credentials=<path>` file holding username/password/domain.
    File { path: String },
    /// Inline username/password/domain, resolved via the secrets domain.
    Inline {
        username: String,
        password: SecretRef,
        #[serde(default)]
        domain: Option<String>,
    },
    /// Guest / anonymous mount (`guest`).
    Guest,
}

/// The backend-specific, validated option set. Each variant carries the grammar
/// its backend understands; `render_options` turns it back into the comma-string
/// the kernel/mount helper consumes. Unknown-but-legal options ride in `extra` so
/// a backend never has to enumerate every kernel option to accept it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "fs")]
pub enum OptionSet {
    /// NFS (`nfs`/`nfs4`) mount options.
    Nfs {
        #[serde(default)]
        vers: Option<String>,
        #[serde(default)]
        hard: Option<bool>,
        #[serde(default)]
        soft: Option<bool>,
        #[serde(default)]
        timeo: Option<u32>,
        #[serde(default)]
        retrans: Option<u32>,
        #[serde(default)]
        actimeo: Option<u32>,
        #[serde(default)]
        rsize: Option<u32>,
        #[serde(default)]
        wsize: Option<u32>,
        /// `_netdev` — kept in the spec but stripped from the autofs map by core.
        #[serde(default, rename = "_netdev")]
        netdev: bool,
        /// Any further raw `key` / `key=value` options, order-preserved.
        #[serde(default)]
        extra: Vec<String>,
    },
    /// SMB/CIFS (`cifs`/`smbfs`) mount options.
    Smb {
        #[serde(default)]
        vers: Option<String>,
        credentials: SmbCredentials,
        #[serde(default)]
        uid: Option<String>,
        #[serde(default)]
        gid: Option<String>,
        #[serde(default)]
        iocharset: Option<String>,
        #[serde(default)]
        noperm: bool,
        #[serde(default)]
        extra: Vec<String>,
    },
    /// Opaque options for a backend with no typed grammar yet: the raw comma
    /// string exactly as supplied. The identity form the default `validate_spec`
    /// produces, so an un-migrated backend behaves precisely as it does today.
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
    /// Serialized remount policy (opaque to the backend; core owns the engine).
    #[serde(default)]
    pub remount_policy: Option<String>,
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
    #[serde(default)]
    pub remount_policy: Option<String>,
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

/// Render a typed [`OptionSet`] into the comma-joined mount-option string. The
/// canonical renderer the default [`StorageBackend::render_options`] delegates to;
/// a backend that wants the standard rendering reuses it rather than re-deriving
/// the option grammar. `Raw` reproduces the declared string verbatim so an
/// un-migrated mount renders byte-identically to core's prior behavior.
pub fn render_option_set(set: &OptionSet) -> String {
    fn push_kv(parts: &mut Vec<String>, key: &str, value: &Option<String>) {
        if let Some(v) = value {
            parts.push(format!("{key}={v}"));
        }
    }
    fn push_num<T: std::fmt::Display>(parts: &mut Vec<String>, key: &str, value: &Option<T>) {
        if let Some(v) = value {
            parts.push(format!("{key}={v}"));
        }
    }

    match set {
        OptionSet::Raw { options } => options.clone().unwrap_or_default(),
        OptionSet::Nfs {
            vers,
            hard,
            soft,
            timeo,
            retrans,
            actimeo,
            rsize,
            wsize,
            netdev,
            extra,
        } => {
            let mut parts = Vec::new();
            push_kv(&mut parts, "vers", vers);
            if *hard == Some(true) {
                parts.push("hard".into());
            }
            if *soft == Some(true) {
                parts.push("soft".into());
            }
            push_num(&mut parts, "timeo", timeo);
            push_num(&mut parts, "retrans", retrans);
            push_num(&mut parts, "actimeo", actimeo);
            push_num(&mut parts, "rsize", rsize);
            push_num(&mut parts, "wsize", wsize);
            if *netdev {
                parts.push("_netdev".into());
            }
            parts.extend(extra.iter().cloned());
            parts.join(",")
        }
        OptionSet::Smb {
            vers,
            credentials,
            uid,
            gid,
            iocharset,
            noperm,
            extra,
        } => {
            let mut parts = Vec::new();
            push_kv(&mut parts, "vers", vers);
            match credentials {
                SmbCredentials::File { path } => parts.push(format!("credentials={path}")),
                SmbCredentials::Inline {
                    username, domain, ..
                } => {
                    parts.push(format!("username={username}"));
                    if let Some(d) = domain {
                        parts.push(format!("domain={d}"));
                    }
                    // The password is a SecretRef the secrets domain resolves at
                    // mount time; it is never rendered into the option string here.
                }
                SmbCredentials::Guest => parts.push("guest".into()),
            }
            push_kv(&mut parts, "uid", uid);
            push_kv(&mut parts, "gid", gid);
            push_kv(&mut parts, "iocharset", iocharset);
            if *noperm {
                parts.push("noperm".into());
            }
            parts.extend(extra.iter().cloned());
            parts.join(",")
        }
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
    register_from_def_styled(name, kind, endpoint, capabilities, "", invoke)
}

/// [`register_from_def`] carrying the backend's mount-style axis from its
/// `BackendDef`. `mount_style` is the raw wire string (`""`/`"kernel_mount"` =
/// kernel, `"userspace_process"` = helper); an unknown value is rejected at load.
/// The zero-axis [`register_from_def`] defaults it to kernel so a caller that
/// doesn't pass the axis keeps its exact prior behavior.
#[cfg(feature = "in-process")]
pub fn register_from_def_styled(
    name: String,
    kind: &str,
    endpoint: String,
    capabilities: &[String],
    mount_style: &str,
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
    fn render_option_set_nfs_orders_and_includes_typed_fields() {
        let set = OptionSet::Nfs {
            vers: Some("4.2".into()),
            hard: Some(true),
            soft: None,
            timeo: Some(600),
            retrans: Some(2),
            actimeo: None,
            rsize: Some(1048576),
            wsize: Some(1048576),
            netdev: true,
            extra: vec!["nconnect=4".into()],
        };
        assert_eq!(
            render_option_set(&set),
            "vers=4.2,hard,timeo=600,retrans=2,rsize=1048576,wsize=1048576,_netdev,nconnect=4"
        );
    }

    #[test]
    fn render_option_set_smb_never_emits_the_password() {
        let set = OptionSet::Smb {
            vers: Some("3.1.1".into()),
            credentials: SmbCredentials::Inline {
                username: "svc".into(),
                password: SecretRef("onepassword://vault/item".into()),
                domain: Some("WORKGROUP".into()),
            },
            uid: Some("1000".into()),
            gid: None,
            iocharset: None,
            noperm: true,
            extra: vec![],
        };
        let rendered = render_option_set(&set);
        assert_eq!(
            rendered,
            "vers=3.1.1,username=svc,domain=WORKGROUP,uid=1000,noperm"
        );
        assert!(!rendered.contains("onepassword"), "secret must not render");
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
            register_from_def_styled("m".into(), "network_share", "e".into(), &[], "weird", thunk)
                .is_err()
        );
    }
}
