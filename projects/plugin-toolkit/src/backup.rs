//! Plugin-side authoring surface for the out-of-process backup bridge — the
//! inverse of the host proxies in `system::backup::proxy`.
//!
//! A plugin contributes a backup **KIND** (the WHAT axis — `vm` / `lxc` /
//! `flash`) by implementing [`BackupKindPlugin`], or a backup **TARGET** (the
//! WHERE axis — `nfs` / `smb` / `s3` / `pbs`) by implementing
//! [`BackupTargetPlugin`]. The host proxy encodes each op's args into a
//! `{invoke_prefix}.{op}` call; [`dispatch_kind_op`] / [`dispatch_target_op`]
//! decode those args against the SAME [`wire`](crate::contract::backup::wire)
//! structs the proxy encoded them with and route to the trait — so a plugin's
//! `invoke` is one call to a dispatch fn here, never a hand-copied per-op match
//! that drifts from the host.
//!
//! ## Sync by design
//!
//! The loader thunk is synchronous and the host proxy already offloads the
//! heavy ops (`backup`/`restore`/`open`/…) onto a blocking pool, so these
//! author methods are plain synchronous fns: do blocking I/O directly.
//!
//! ## Shared-filesystem model
//!
//! A loaded plugin runs as a subprocess on the SAME host. So a KIND writes its
//! backup INTO the host-provided `payload_dir` and a TARGET returns a host-local
//! root path from [`open`](BackupTargetPlugin::open); only small JSON metadata
//! crosses the seam. No backup bytes are serialized over the wire.

use std::path::Path;

use crate::contract::backup::wire::{
    FitsArgs, InstanceArgs, NameArgs, OP_AVAILABLE, OP_BACKING_KEY, OP_BACKUP,
    OP_DEFAULT_RETENTION, OP_DEFAULT_SCHEDULE, OP_FITS, OP_INSTANCES, OP_LAYOUT, OP_OPEN,
    OP_REFRESH, OP_RESTORE, OP_SYNC, OP_TITLE, OpenReply, PayloadArgs,
};
use crate::contract::backup::{
    BackupOutcome, BackupSchedule, Placement, Retention, TargetLocation,
};

/// One backup KIND, authored plugin-side. Mirrors the host's `BackupProvider`;
/// the defaults match the host proxy's fallbacks so an author overrides only
/// what the kind actually needs.
pub trait BackupKindPlugin: Send + Sync {
    /// Kind name (`"vm"`, `"lxc"`, `"flash"`). Must equal the `kind` the plugin
    /// advertised in its `backup_kind` backend def.
    fn kind(&self) -> &str;

    /// Human-facing title for listings. Defaults to the kind.
    fn title(&self) -> String {
        self.kind().to_string()
    }

    /// Instances this kind can back up. Single-instance kinds keep the default
    /// `["default"]`; a multi-instance kind (every VM on a node) overrides.
    /// Returning `Err` propagates a failed enumeration rather than masquerading
    /// as one bogus instance.
    fn instances(&self) -> Result<Vec<String>, String> {
        Ok(vec!["default".to_string()])
    }

    /// The `<category>/<class>/<name>` layout segments this instance is filed
    /// under. Defaults to the flat `[kind, instance]`.
    fn layout(&self, instance: &str) -> Vec<String> {
        vec![self.kind().to_string(), instance.to_string()]
    }

    /// Capture `instance`'s state into `payload_dir` (host-provided, empty).
    /// Write files under it and return metadata; `Err` aborts the slot.
    fn backup(&self, payload_dir: &Path, instance: &str) -> Result<BackupOutcome, String>;

    /// Restore `instance` from a previously produced `payload_dir`.
    fn restore(&self, payload_dir: &Path, instance: &str) -> Result<(), String>;
}

/// One backup TARGET, authored plugin-side. Mirrors the host's
/// `BackupTargetProvider`; defaults match the host proxy's fallbacks.
pub trait BackupTargetPlugin: Send + Sync {
    /// Target kind name (`"nfs"`, `"smb"`, `"s3"`, `"pbs"`). Must equal the
    /// advertised `kind`.
    fn kind(&self) -> &str;

    /// Human-facing title for listings. Defaults to the kind.
    fn title(&self) -> String {
        self.kind().to_string()
    }

    /// Resolve the named target instance to a host-local root path, provisioning
    /// storage as needed. The generic store owns layout/retention beneath it.
    fn open(&self, name: &str) -> Result<String, String>;

    /// Push local writes to the remote backing after a run committed (git push,
    /// s3 upload). Default: nothing to do (a plain local path is already durable).
    fn sync(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    /// Pull the remote backing into the local tree before a list/restore reads
    /// it (git pull, s3 download). Default: nothing to do.
    fn refresh(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    /// Whether this target is eligible for a workload at `placement`. Default
    /// fits everywhere (as `local` does). Only gates what is OFFERED.
    fn fits(&self, _placement: &Placement) -> bool {
        true
    }

    /// This target's default retention, or `None` to fall through to the unit
    /// policy default.
    fn default_retention(&self, _name: &str) -> Option<Retention> {
        None
    }

    /// This target's default schedule, or `None` to fall through to the unit
    /// policy default.
    fn default_schedule(&self, _name: &str) -> Option<BackupSchedule> {
        None
    }

    /// Concrete storage locations this kind exposes for the target picker.
    /// Default: none discoverable.
    fn available(&self) -> Result<Vec<TargetLocation>, String> {
        Ok(Vec::new())
    }

    /// Globally stable backing identity for the named instance, for fleet-wide
    /// collision detection. Default `<kind>://<name>`; override for per-host or
    /// shared storage (`local://<host>`, `nfs://server/export`).
    fn backing_key(&self, name: &str) -> Result<String, String> {
        Ok(format!("{}://{}", self.kind(), name))
    }
}

// The KIND/TARGET dispatch is the erased-invoke seam — args/results/errors are
// `serde_json::Value` (the wire's own type; no String hop). Scoped here.
#[allow(clippy::disallowed_types)]
fn enc<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|e| serde_json::Value::String(format!("failed to encode result: {e}")))
}

#[allow(clippy::disallowed_types)]
fn dec<T: serde::de::DeserializeOwned>(
    op: &str,
    args: serde_json::Value,
) -> Result<T, serde_json::Value> {
    serde_json::from_value(args)
        .map_err(|e| serde_json::Value::String(format!("invalid `{op}` args: {e}")))
}

/// Plugin-side inverse of the host `BackupKindProxy`: decode a proxied KIND op's
/// JSON args and route it to an in-process [`BackupKindPlugin`], returning the
/// op's JSON-encoded result (or an error string). `op` is the bare op name (the
/// loader thunk strips the invoke prefix first).
#[allow(clippy::disallowed_types)] // erased-invoke dispatch seam — Value in/out.
pub fn dispatch_kind_op(
    plugin: &dyn BackupKindPlugin,
    op: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    match op {
        OP_TITLE => enc(&plugin.title()),
        OP_INSTANCES => enc(&plugin.instances().map_err(serde_json::Value::String)?),
        OP_LAYOUT => {
            let a: InstanceArgs = dec(op, args)?;
            enc(&plugin.layout(&a.instance))
        }
        OP_BACKUP => {
            let a: PayloadArgs = dec(op, args)?;
            enc(&plugin
                .backup(Path::new(&a.payload_dir), &a.instance)
                .map_err(serde_json::Value::String)?)
        }
        OP_RESTORE => {
            let a: PayloadArgs = dec(op, args)?;
            plugin
                .restore(Path::new(&a.payload_dir), &a.instance)
                .map_err(serde_json::Value::String)?;
            Ok(serde_json::Value::Null)
        }
        other => Err(serde_json::Value::String(format!(
            "backup kind has no operation '{other}'"
        ))),
    }
}

/// Plugin-side inverse of the host `BackupTargetProxy`: decode a proxied TARGET
/// op's JSON args and route it to an in-process [`BackupTargetPlugin`].
#[allow(clippy::disallowed_types)] // erased-invoke dispatch seam — Value in/out.
pub fn dispatch_target_op(
    plugin: &dyn BackupTargetPlugin,
    op: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    match op {
        OP_TITLE => enc(&plugin.title()),
        OP_OPEN => {
            let a: NameArgs = dec(op, args)?;
            enc(&OpenReply {
                root: plugin.open(&a.name).map_err(serde_json::Value::String)?,
            })
        }
        OP_SYNC => {
            let a: NameArgs = dec(op, args)?;
            plugin.sync(&a.name).map_err(serde_json::Value::String)?;
            Ok(serde_json::Value::Null)
        }
        OP_REFRESH => {
            let a: NameArgs = dec(op, args)?;
            plugin.refresh(&a.name).map_err(serde_json::Value::String)?;
            Ok(serde_json::Value::Null)
        }
        OP_FITS => {
            let a: FitsArgs = dec(op, args)?;
            enc(&plugin.fits(&a.placement))
        }
        OP_DEFAULT_RETENTION => {
            let a: NameArgs = dec(op, args)?;
            enc(&plugin.default_retention(&a.name))
        }
        OP_DEFAULT_SCHEDULE => {
            let a: NameArgs = dec(op, args)?;
            enc(&plugin.default_schedule(&a.name))
        }
        OP_AVAILABLE => enc(&plugin.available().map_err(serde_json::Value::String)?),
        OP_BACKING_KEY => {
            let a: NameArgs = dec(op, args)?;
            enc(&plugin
                .backing_key(&a.name)
                .map_err(serde_json::Value::String)?)
        }
        other => Err(serde_json::Value::String(format!(
            "backup target has no operation '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeKind;
    impl BackupKindPlugin for FakeKind {
        fn kind(&self) -> &str {
            "vm"
        }
        fn title(&self) -> String {
            "Virtual Machines".to_string()
        }
        fn instances(&self) -> Result<Vec<String>, String> {
            Ok(vec!["100".to_string(), "101".to_string()])
        }
        fn backup(&self, _dir: &Path, instance: &str) -> Result<BackupOutcome, String> {
            Ok(BackupOutcome {
                checksum: None,
                note: Some(format!("captured {instance}")),
            })
        }
        fn restore(&self, _dir: &Path, _instance: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn kind_dispatch_routes_ops_and_defaults() {
        let p = FakeKind;
        // title + instances come back as JSON values.
        assert_eq!(
            dispatch_kind_op(&p, OP_TITLE, serde_json::json!({})).unwrap(),
            serde_json::json!("Virtual Machines")
        );
        assert_eq!(
            dispatch_kind_op(&p, OP_INSTANCES, serde_json::json!({})).unwrap(),
            serde_json::json!(["100", "101"])
        );
        // layout uses the default flat [kind, instance] and decodes InstanceArgs.
        assert_eq!(
            dispatch_kind_op(&p, OP_LAYOUT, serde_json::json!({"instance":"100"})).unwrap(),
            serde_json::json!(["vm", "100"])
        );
        // backup decodes PayloadArgs and returns the outcome; restore → null.
        let out = dispatch_kind_op(
            &p,
            OP_BACKUP,
            serde_json::json!({"payload_dir":"/t","instance":"100"}),
        )
        .unwrap();
        assert!(out.to_string().contains("captured 100"));
        assert_eq!(
            dispatch_kind_op(
                &p,
                OP_RESTORE,
                serde_json::json!({"payload_dir":"/t","instance":"100"})
            )
            .unwrap(),
            serde_json::Value::Null
        );
        assert!(dispatch_kind_op(&p, "bogus", serde_json::json!({})).is_err());
    }

    struct FakeTarget;
    impl BackupTargetPlugin for FakeTarget {
        fn kind(&self) -> &str {
            "pbs"
        }
        fn open(&self, name: &str) -> Result<String, String> {
            Ok(format!("/mnt/pbs/{name}"))
        }
    }

    #[test]
    fn target_dispatch_open_and_defaults() {
        let p = FakeTarget;
        // open decodes NameArgs and re-encodes as OpenReply { root }.
        assert_eq!(
            dispatch_target_op(&p, OP_OPEN, serde_json::json!({"name":"default"})).unwrap(),
            serde_json::json!({"root":"/mnt/pbs/default"})
        );
        // default fits=true, backing_key = <kind>://<name>, available = [].
        assert_eq!(
            dispatch_target_op(&p, OP_FITS, serde_json::json!({"placement":{"labels":[]}}))
                .unwrap(),
            serde_json::json!(true)
        );
        assert_eq!(
            dispatch_target_op(&p, OP_BACKING_KEY, serde_json::json!({"name":"default"})).unwrap(),
            serde_json::json!("pbs://default")
        );
        assert_eq!(
            dispatch_target_op(&p, OP_AVAILABLE, serde_json::json!({})).unwrap(),
            serde_json::json!([])
        );
        assert_eq!(
            dispatch_target_op(&p, OP_SYNC, serde_json::json!({"name":"default"})).unwrap(),
            serde_json::Value::Null
        );
    }
}
