//! Managed-mount declarative store — orca-native source of truth for the
//! network / disk / object mounts orca owns.
//!
//! Each row is a full mount spec: which registered storage backend mounts it,
//! where it comes from, where it lands, and (Slice 3) its remount policy. The
//! `storage.mount` execution path resolves a row here, fetches its credential
//! via the secrets domain, and drives the backend's mount.
//!
//! Rides `#[endpoint_resource]` so the registry layer — a SQLite table plus the
//! five CRUD verbs across CLI / MCP / REST — is generated identically to every
//! other managed resource ([[feedback-plugin-toolkit-max-power-min-boilerplate]]).
//! Generates `storage_mount.{list,detail,create,update,delete}` over the
//! `managed_mounts` table. The `credential` field is `#[secret]`: persisted but
//! never surfaced in read output (it appears only as `has_credential: bool`).

use plugin_toolkit::endpoint_resource;

/// A mount orca manages declaratively. `name` (PK) and `enabled` are implicit,
/// supplied by the macro; the data fields below carry the full mount spec.
#[endpoint_resource(plugin = "storage_mount", table = "managed_mounts")]
pub struct ManagedMount {
    pub name: String,
    /// Registered storage backend that mounts this entry (`nfs`, `smb`, …);
    /// resolved against the process-global storage registry at mount time.
    pub backend: String,
    /// Storage kind for display/grouping: `network_share` | `disk` | `object`.
    pub kind: String,
    /// Mount source as the backend expects it: `host:/export` (NFS),
    /// `//server/share` (SMB), `s3://bucket/prefix` (object), …
    pub source: String,
    /// Absolute mountpoint / target path.
    pub target: String,
    /// Filesystem / transport type (`nfs4`, `cifs`, `smbfs`, …).
    pub fstype: String,
    /// Extra mount options, comma-joined (`vers=4.2,nofail`). Optional.
    pub options: Option<String>,
    /// Credential reference — a SecretRef the secrets domain resolves
    /// (`onepassword://…`, `bitwarden://…`, or a native secret id). Stored,
    /// never surfaced.
    #[secret]
    pub credential: Option<String>,
    /// Serialized remount policy (Slice 3: always | schedule | backoff |
    /// manual). Optional until the policy engine lands.
    pub remount_policy: Option<String>,
    pub enabled: bool,
}
