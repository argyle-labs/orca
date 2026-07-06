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
    /// Ordered failover sources (secondaries), newline-separated, in priority
    /// order. The primary is `source`; these are tried after it when the primary
    /// is stale/unreachable. Optional — NULL means single-source (today's
    /// behavior). Consume via [`ordered_sources`] — never parse ad hoc.
    pub failover_sources: Option<String>,
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

/// Resolve a mount's sources into a single priority-ordered list: the primary
/// (`source`) first, then each non-empty, trimmed line of `failover_sources`.
///
/// This is the one place the `failover_sources` string is parsed — consumers
/// (the mount/remount exec path, `recover_stale` failover selection) take the
/// returned `Vec` and never touch the raw string. A `None` / blank
/// `failover_sources` yields a single-element list, preserving today's
/// single-source behavior exactly.
pub fn ordered_sources(source: &str, failover_sources: Option<&str>) -> Vec<String> {
    let mut sources = vec![source.to_string()];
    if let Some(raw) = failover_sources {
        sources.extend(
            raw.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::ordered_sources;

    #[test]
    fn single_source_when_no_failovers() {
        assert_eq!(
            ordered_sources("primary:/srv/pool/data", None),
            ["primary:/srv/pool/data"]
        );
    }

    #[test]
    fn primary_first_then_trimmed_nonempty_lines() {
        let failovers = "  secondary:/srv/pool/data \n\n tertiary:/srv/pool/data\n";
        assert_eq!(
            ordered_sources("primary:/srv/pool/data", Some(failovers)),
            [
                "primary:/srv/pool/data",
                "secondary:/srv/pool/data",
                "tertiary:/srv/pool/data",
            ]
        );
    }

    #[test]
    fn blank_failovers_is_single_source() {
        assert_eq!(ordered_sources("a", Some("   \n  \n")), ["a"]);
    }

    // Regression for the in-core `endpoint_resource!` DB path. `managed_mounts`
    // is compiled INTO the daemon, not loaded as a cdylib, so no plugin loader
    // ever installs a `HOST_DB` channel. Before the in-core fallback added to
    // `plugin_toolkit::runtime::db_op`, `endpoint_db::list()` failed on a real
    // daemon with "core DB service not installed (daemon predates set_host?)" —
    // which broke `storage.mount`, `storage.recover`, and the self-heal loop.
    #[test]
    fn endpoint_db_list_works_in_core_without_host_channel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mounts.db");
        db::with_thread_db_path(&path, || {
            // The `managed_mounts` table is a SchemaFragment, applied separately
            // from `apply_schema`; mirror the daemon-boot reconcile so it exists.
            let conn = db::open_default().expect("open temp db");
            db::schema_fragments::apply_fragments(&conn).expect("apply fragments");
            drop(conn);

            let rows = super::endpoint_db::list().expect("endpoint_db::list in-core");
            assert!(rows.is_empty(), "a fresh db has no managed mounts");
        });
    }
}
