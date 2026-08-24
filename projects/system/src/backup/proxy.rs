//! Out-of-process bridge for the two backup axes — the host-side proxies that
//! let a subprocess plugin contribute a backup KIND ([`BackupProvider`]) or a
//! backup TARGET ([`BackupTargetProvider`]) over the loader's JSON `invoke`
//! boundary, exactly as `diagnostics`/`ups`/`service` do for their domains.
//!
//! ## Why this lives here (and not in the loader)
//!
//! The two registries live in `system` (this crate), which sits *downstream* of
//! `plugin-loader`; the loader therefore cannot name them in its hardcoded
//! dispatch match without a dependency cycle. So `system` injects these
//! constructors into the loader at startup via
//! [`plugin_loader::register_domain_constructor`] (see [`install`]); the loader
//! consults that extension table as the fallthrough of its dispatch, and an
//! injected domain then loads/unloads exactly like a built-in one.
//!
//! ## Shared-filesystem model
//!
//! A loaded plugin runs as a **subprocess on the same host**, sharing its
//! filesystem. So a KIND's `payload_dir` and a TARGET's store root cross the
//! boundary as path STRINGS: the host allocates/created the directory, the
//! plugin subprocess reads/writes it directly, and only small JSON metadata
//! travels over `invoke`. No backup bytes are serialized across the seam.
//!
//! No `async_trait` macro ([[no-async-trait-macro]]): async methods return the
//! hand-desugared [`contract::BoxFuture`], as the in-process providers do.

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`
// — the loader thunk already produces a parsed value, so there is no String hop.
// This module names that type only at that FFI seam — the sanctioned opaque
// boundary, scoped here, mirroring `containers::ffi` and `contract::diagnostics`.
#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use anyhow::{Result, anyhow};
use contract::backup::wire::{
    DOMAIN_KIND, DOMAIN_TARGET, FitsArgs, InstanceArgs, NameArgs, OP_AVAILABLE, OP_BACKING_KEY,
    OP_BACKUP, OP_DEFAULT_RETENTION, OP_DEFAULT_SCHEDULE, OP_FITS, OP_INSTANCES, OP_LAYOUT,
    OP_OPEN, OP_REFRESH, OP_RESTORE, OP_SYNC, OP_TITLE, OpenReply, PayloadArgs,
};
use contract::backup::{BackupSchedule, Placement, Retention};
use contract::{BoxFuture, ToolCtx};
use plugin_loader::BackendInvoke;
use plugin_toolkit::abi::BackendDef;
use serde::Deserialize;

use super::provider::{self, BackupOutcome, BackupProvider};
use super::store::BackupStore;
use super::target::{self, BackupTargetProvider, TargetLocation};

/// Install the backup KIND + TARGET domain constructors into the plugin loader.
/// Call once at daemon startup, before plugins load — alongside
/// [`super::tools::register_builtin_providers`].
pub fn install() {
    plugin_loader::register_domain_constructor(
        DOMAIN_KIND,
        register_kind_from_def,
        deregister_kind,
    );
    plugin_loader::register_domain_constructor(
        DOMAIN_TARGET,
        register_target_from_def,
        deregister_target_by_kind,
    );
}

// ── Ownership guard (collision protection) ──────────────────────────────────
//
// The KIND/TARGET registries replace by kind, so two DIFFERENT plugins declaring
// the same kind would collapse to one entry — and either unloading would
// deregister it for both. These maps record which plugin owns each kind (by its
// `invoke_prefix`, the plugin identity), so the same plugin reloading replaces
// while a different plugin — or a built-in already holding that kind — is
// rejected loudly.
static KIND_OWNERS: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static TARGET_OWNERS: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Claim `kind` for `owner` (the plugin's `invoke_prefix`) in `owners`, or error
/// on collision: a different plugin already owns it, or — when unowned but
/// `builtin_present` — a built-in provider holds the kind. On success records
/// ownership. `domain` names the axis for the error message (`kind`/`target`).
fn claim_owner(
    owners: &RwLock<HashMap<String, String>>,
    domain: &str,
    kind: &str,
    owner: &str,
    builtin_present: bool,
) -> Result<()> {
    let mut g = owners.write().expect("backup owner registry poisoned");
    match g.get(kind) {
        // Same plugin re-registering (reload) → replace is fine.
        Some(existing) if existing == owner => {}
        Some(existing) => {
            return Err(anyhow!(
                "backup {domain} '{kind}' already registered by plugin '{existing}'"
            ));
        }
        None if builtin_present => {
            return Err(anyhow!(
                "backup {domain} '{kind}' collides with a built-in provider"
            ));
        }
        None => {}
    }
    g.insert(kind.to_string(), owner.to_string());
    Ok(())
}

/// Fetch the plugin-supplied human title once at registration, over the wire's
/// [`OP_TITLE`] op. Falls back to the kind when the op is absent or errors —
/// a plugin that predates the title op simply reports its kind. Kept a
/// registration-time value (not a per-call wire round-trip) so the sync
/// `title(&self) -> &str` trait method can return a borrow.
fn fetch_title(invoke: &BackendInvoke, kind: &str) -> String {
    invoke(OP_TITLE, serde_json::json!({}))
        .ok()
        .and_then(|s| serde_json::from_value::<String>(s).ok())
        .unwrap_or_else(|| kind.to_string())
}

// ── KIND proxy ────────────────────────────────────────────────────────────────

/// Build and register a [`BackupProvider`] from a plugin descriptor plus its
/// invoke thunk. Loader dispatch entry for `domain = "backup_kind"`; `def.kind`
/// is the KIND name (`"vm"`, `"lxc"`, `"flash"`).
pub fn register_kind_from_def(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    if def.kind.is_empty() {
        return Err(anyhow!("backup_kind backend '{}' has empty kind", def.name));
    }
    // The loader records this backend by `def.name` and, on unload, deregisters
    // by that name; the provider registry keys by `kind`. Enforce `name == kind`
    // so teardown is correct by construction rather than relying on the author
    // helper — a hand-rolled BackendDef with `name != kind` would otherwise
    // orphan the provider on unload. The canonical `backup_kind_backend_def`
    // helper already sets both equal ([[backup-oop-teardown-key-mismatch]]).
    if def.name != def.kind {
        return Err(anyhow!(
            "backup_kind backend name '{}' must equal kind '{}' (teardown keys on name)",
            def.name,
            def.kind
        ));
    }
    // Reject a collision before mutating the registry: a different plugin, or a
    // built-in kind (host/service), already owning this kind.
    claim_owner(
        &KIND_OWNERS,
        "kind",
        &def.kind,
        &def.invoke_prefix,
        provider::provider(&def.kind).is_some(),
    )?;
    let title = fetch_title(&invoke, &def.kind);
    provider::register_provider(Arc::new(BackupKindProxy {
        kind: def.kind.clone(),
        title,
        invoke,
    }));
    Ok(())
}

fn deregister_kind(kind: &str) {
    provider::deregister_provider(kind);
    KIND_OWNERS
        .write()
        .expect("backup owner registry poisoned")
        .remove(kind);
}

/// A [`BackupProvider`] backed by a subprocess plugin over the JSON-proxy seam.
struct BackupKindProxy {
    kind: String,
    /// Plugin-supplied title, fetched once at registration ([`fetch_title`]).
    title: String,
    invoke: BackendInvoke,
}

impl BackupKindProxy {
    /// Synchronous metadata call — the loader thunk is itself synchronous, so a
    /// sync trait method (`instances`/`layout`) drives it directly. Must stay
    /// cheap on the plugin side: it is not offloaded to a blocking pool.
    fn call_sync<T: for<'de> Deserialize<'de>>(
        &self,
        op: &str,
        args: serde_json::Value,
    ) -> Result<T> {
        let out = (self.invoke)(op, args)
            .map_err(|e| anyhow!("backup kind '{}' {op} failed: {e}", self.kind))?;
        serde_json::from_value(out).map_err(|e| {
            anyhow!(
                "backup kind '{}' {op} returned invalid JSON: {e}",
                self.kind
            )
        })
    }

    /// Async op offloaded to the blocking pool, as the diagnostics proxy does.
    fn call_async<T: for<'de> Deserialize<'de> + Send + 'static>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args))
                .await
                .map_err(|e| anyhow!("backup kind '{kind}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow!("backup kind '{kind}' {op} failed: {e}"))?;
            serde_json::from_value(out)
                .map_err(|e| anyhow!("backup kind '{kind}' {op} returned invalid JSON: {e}"))
        })
    }

    /// A `()`-returning async op: the plugin's reply body (`null` / `{}` / empty)
    /// is ignored — only success/failure matters.
    fn call_async_unit(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<()>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || invoke(op, args))
                .await
                .map_err(|e| anyhow!("backup kind '{kind}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow!("backup kind '{kind}' {op} failed: {e}"))?;
            Ok(())
        })
    }
}

impl BackupProvider for BackupKindProxy {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn instances(&self) -> Result<Vec<String>> {
        // Propagate a failed enumeration — never fabricate `["default"]`. This
        // kind may be multi-instance (e.g. every VM on a proxmox node); a
        // fabricated singleton would silently skip every real instance while the
        // run reports success. The caller records the error and skips this kind.
        self.call_sync::<Vec<String>>(OP_INSTANCES, serde_json::json!({}))
    }

    fn layout(&self, instance: &str) -> Vec<String> {
        let args = serde_json::to_value(&InstanceArgs {
            instance: instance.to_string(),
        })
        .unwrap_or_default();
        match self.call_sync::<Vec<String>>(OP_LAYOUT, args) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("{e}; falling back to flat [kind, instance] layout");
                vec![self.kind.clone(), instance.to_string()]
            }
        }
    }

    fn backup<'a>(
        &'a self,
        payload_dir: &'a Path,
        instance: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<BackupOutcome>> {
        let args = serde_json::to_value(&PayloadArgs {
            payload_dir: payload_dir.to_string_lossy().into_owned(),
            instance: instance.to_string(),
        })
        .unwrap_or_default();
        self.call_async(OP_BACKUP, args)
    }

    fn restore<'a>(
        &'a self,
        payload_dir: &'a Path,
        instance: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<()>> {
        let args = serde_json::to_value(&PayloadArgs {
            payload_dir: payload_dir.to_string_lossy().into_owned(),
            instance: instance.to_string(),
        })
        .unwrap_or_default();
        self.call_async_unit(OP_RESTORE, args)
    }
}

// ── TARGET proxy ──────────────────────────────────────────────────────────────

/// Build and register a [`BackupTargetProvider`] from a plugin descriptor plus
/// its invoke thunk. Loader dispatch entry for `domain = "backup_target"`;
/// `def.kind` is the TARGET kind (`"nfs"`, `"smb"`, `"s3"`, `"pbs"`).
pub fn register_target_from_def(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    if def.kind.is_empty() {
        return Err(anyhow!(
            "backup_target backend '{}' has empty kind",
            def.name
        ));
    }
    // See register_kind_from_def: teardown deregisters by the loader-recorded
    // `name`, but the target registry keys by `kind`. Enforce `name == kind` so
    // unload cannot orphan a target ([[backup-oop-teardown-key-mismatch]]).
    if def.name != def.kind {
        return Err(anyhow!(
            "backup_target backend name '{}' must equal kind '{}' (teardown keys on name)",
            def.name,
            def.kind
        ));
    }
    claim_owner(
        &TARGET_OWNERS,
        "target",
        &def.kind,
        &def.invoke_prefix,
        target::target(&def.kind).is_some(),
    )?;
    let title = fetch_title(&invoke, &def.kind);
    target::register_target(Arc::new(BackupTargetProxy {
        kind: def.kind.clone(),
        title,
        invoke,
    }));
    Ok(())
}

fn deregister_target_by_kind(kind: &str) {
    target::deregister_target(kind);
    TARGET_OWNERS
        .write()
        .expect("backup owner registry poisoned")
        .remove(kind);
}

/// A [`BackupTargetProvider`] backed by a subprocess plugin over the JSON-proxy
/// seam. `open` resolves to a host-local root path the plugin provisioned; the
/// generic [`BackupStore`] then owns layout/listing/retention beneath it.
struct BackupTargetProxy {
    kind: String,
    /// Plugin-supplied title, fetched once at registration ([`fetch_title`]).
    title: String,
    invoke: BackendInvoke,
}

impl BackupTargetProxy {
    fn call_sync<T: for<'de> Deserialize<'de>>(
        &self,
        op: &str,
        args: serde_json::Value,
    ) -> Result<T> {
        let out = (self.invoke)(op, args)
            .map_err(|e| anyhow!("backup target '{}' {op} failed: {e}", self.kind))?;
        serde_json::from_value(out).map_err(|e| {
            anyhow!(
                "backup target '{}' {op} returned invalid JSON: {e}",
                self.kind
            )
        })
    }

    fn call_async<T: for<'de> Deserialize<'de> + Send + 'static>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args))
                .await
                .map_err(|e| anyhow!("backup target '{kind}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow!("backup target '{kind}' {op} failed: {e}"))?;
            serde_json::from_value(out)
                .map_err(|e| anyhow!("backup target '{kind}' {op} returned invalid JSON: {e}"))
        })
    }

    /// A `()`-returning async op: the plugin's reply body is ignored.
    fn call_async_unit(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<()>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || invoke(op, args))
                .await
                .map_err(|e| anyhow!("backup target '{kind}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow!("backup target '{kind}' {op} failed: {e}"))?;
            Ok(())
        })
    }
}

impl BackupTargetProvider for BackupTargetProxy {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn open<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<BackupStore>> {
        let args = serde_json::to_value(&NameArgs {
            name: name.to_string(),
        })
        .unwrap_or_default();
        Box::pin(async move {
            let reply: OpenReply = self.call_async(OP_OPEN, args).await?;
            Ok(BackupStore::new(PathBuf::from(reply.root)))
        })
    }

    fn sync<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<()>> {
        let args = serde_json::to_value(&NameArgs {
            name: name.to_string(),
        })
        .unwrap_or_default();
        self.call_async_unit(OP_SYNC, args)
    }

    fn refresh<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<()>> {
        let args = serde_json::to_value(&NameArgs {
            name: name.to_string(),
        })
        .unwrap_or_default();
        self.call_async_unit(OP_REFRESH, args)
    }

    fn fits(&self, placement: &Placement) -> bool {
        let args = serde_json::to_value(&FitsArgs {
            placement: placement.clone(),
        })
        .unwrap_or_default();
        match self.call_sync::<bool>(OP_FITS, args) {
            Ok(v) => v,
            Err(e) => {
                // Advisory gate only; default to offering (as `local` does).
                tracing::warn!("{e}; defaulting fits=true");
                true
            }
        }
    }

    fn default_retention(&self, name: &str) -> Option<Retention> {
        let args = serde_json::to_value(&NameArgs {
            name: name.to_string(),
        })
        .unwrap_or_default();
        self.call_sync::<Option<Retention>>(OP_DEFAULT_RETENTION, args)
            .unwrap_or_else(|e| {
                tracing::warn!("{e}; falling back to policy default");
                None
            })
    }

    fn default_schedule(&self, name: &str) -> Option<BackupSchedule> {
        let args = serde_json::to_value(&NameArgs {
            name: name.to_string(),
        })
        .unwrap_or_default();
        self.call_sync::<Option<BackupSchedule>>(OP_DEFAULT_SCHEDULE, args)
            .unwrap_or_else(|e| {
                tracing::warn!("{e}; falling back to policy default");
                None
            })
    }

    fn available<'a>(&'a self, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Vec<TargetLocation>>> {
        self.call_async(OP_AVAILABLE, serde_json::json!({}))
    }

    fn backing_key<'a>(
        &'a self,
        name: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<String>> {
        let args = serde_json::to_value(&NameArgs {
            name: name.to_string(),
        })
        .unwrap_or_default();
        self.call_async(OP_BACKING_KEY, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        }))
    }

    /// A fake invoke thunk recording ops and returning canned JSON per op.
    fn thunk(
        responses: std::collections::HashMap<&'static str, serde_json::Value>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> BackendInvoke {
        Arc::new(move |op: &str, args: serde_json::Value| {
            seen.lock().unwrap().push(format!("{op}:{args}"));
            responses
                .get(op)
                .cloned()
                .ok_or_else(|| serde_json::Value::String(format!("no canned response for {op}")))
        })
    }

    #[test]
    fn kind_proxy_metadata_and_fallbacks() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_INSTANCES, serde_json::json!(["a", "b"]));
        // No layout response → must fall back to [kind, instance].
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(r, seen),
        };
        assert_eq!(p.kind(), "vm");
        assert_eq!(
            p.instances().unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            p.layout("100"),
            vec!["vm".to_string(), "100".to_string()],
            "missing layout op falls back to flat layout"
        );
    }

    #[test]
    fn kind_proxy_instances_surfaces_enumeration_error() {
        // A failed `instances` op must be an Err, NOT a fabricated ["default"] —
        // otherwise a multi-instance kind silently backs up nothing real.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(std::collections::HashMap::new(), seen),
        };
        assert!(p.instances().is_err(), "enumeration failure must surface");
    }

    #[tokio::test]
    async fn target_proxy_open_wraps_returned_root() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_OPEN, serde_json::json!({"root":"/mnt/nas/backups"}));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, seen.clone()),
        };
        let store = p.open("default", &ctx()).await.unwrap();
        assert_eq!(store.root(), Path::new("/mnt/nas/backups"));
        assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("open:")));
    }

    #[test]
    fn target_proxy_fits_defaults_true_on_error() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(std::collections::HashMap::new(), seen),
        };
        assert!(
            p.fits(&Placement::bare()),
            "advisory gate defaults to offering"
        );
    }

    #[test]
    fn register_from_def_rejects_empty_kind() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let invoke = thunk(std::collections::HashMap::new(), seen);
        let def = BackendDef {
            domain: DOMAIN_KIND.to_string(),
            ..Default::default()
        };
        assert!(register_kind_from_def(&def, invoke).is_err());
    }

    #[test]
    fn register_kind_rejects_name_kind_mismatch() {
        // Teardown deregisters by name but the registry keys by kind — a
        // divergent def would orphan the provider on unload, so it is rejected.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let def = BackendDef {
            domain: DOMAIN_KIND.to_string(),
            name: "proxmox".to_string(),
            kind: "vm".to_string(),
            ..Default::default()
        };
        let err = register_kind_from_def(&def, thunk(Default::default(), seen))
            .expect_err("name != kind must be rejected");
        assert!(err.to_string().contains("must equal kind"), "{err}");
    }

    #[test]
    fn register_target_rejects_name_kind_mismatch() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let def = BackendDef {
            domain: DOMAIN_TARGET.to_string(),
            name: "my-nas".to_string(),
            kind: "nfs".to_string(),
            ..Default::default()
        };
        let err = register_target_from_def(&def, thunk(Default::default(), seen))
            .expect_err("name != kind must be rejected");
        assert!(err.to_string().contains("must equal kind"), "{err}");
    }

    #[test]
    fn register_kind_accepts_name_equals_kind() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let def = BackendDef {
            domain: DOMAIN_KIND.to_string(),
            name: "acc-kind".to_string(),
            kind: "acc-kind".to_string(),
            invoke_prefix: "acc.plugin".to_string(),
            ..Default::default()
        };
        register_kind_from_def(&def, thunk(Default::default(), seen))
            .expect("name == kind is accepted");
        // Clean up the process-global registry + owner map via the real teardown.
        deregister_kind("acc-kind");
    }

    fn kind_def(kind: &str, owner: &str) -> BackendDef {
        BackendDef {
            domain: DOMAIN_KIND.to_string(),
            name: kind.to_string(),
            kind: kind.to_string(),
            invoke_prefix: owner.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn register_kind_rejects_second_plugin_claiming_same_kind() {
        // Plugin A registers kind "dupvm"; a DIFFERENT plugin B claiming the same
        // kind is rejected, so B's load can't silently replace A's provider.
        let a = kind_def("dupvm", "pluginA.backup");
        register_kind_from_def(
            &a,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect("first registration succeeds");

        let b = kind_def("dupvm", "pluginB.backup");
        let err = register_kind_from_def(
            &b,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect_err("second plugin on same kind must be rejected");
        assert!(
            err.to_string().contains("already registered by plugin"),
            "{err}"
        );

        // Same plugin A re-registering (reload) is allowed.
        register_kind_from_def(
            &a,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect("same plugin reload replaces");

        deregister_kind("dupvm");
    }

    #[test]
    fn register_kind_rejects_collision_with_builtin() {
        // A built-in provider (registered directly, not via the OOP path) owns
        // "host"; a plugin claiming "host" is rejected rather than replacing it.
        provider::register_provider(Arc::new(super::super::host::HostBackupProvider::new()));
        let def = kind_def("host", "evil.plugin");
        let err = register_kind_from_def(
            &def,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect_err("must not override a built-in kind");
        assert!(err.to_string().contains("built-in"), "{err}");
        provider::deregister_provider("host");
    }

    // ── fetch_title ─────────────────────────────────────────────────────────

    #[test]
    fn fetch_title_returns_plugin_supplied_title() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_TITLE, serde_json::json!("Proxmox VM"));
        let invoke = thunk(r, Arc::new(Mutex::new(Vec::new())));
        assert_eq!(fetch_title(&invoke, "vm"), "Proxmox VM");
    }

    #[test]
    fn fetch_title_falls_back_to_kind_when_op_absent() {
        // A plugin predating the title op → invoke errors → fall back to kind.
        let invoke = thunk(Default::default(), Arc::new(Mutex::new(Vec::new())));
        assert_eq!(fetch_title(&invoke, "lxc"), "lxc");
    }

    #[test]
    fn fetch_title_falls_back_when_reply_not_a_string() {
        // A non-string reply body cannot deserialize to String → fall back.
        let mut r = std::collections::HashMap::new();
        r.insert(OP_TITLE, serde_json::json!({"unexpected": true}));
        let invoke = thunk(r, Arc::new(Mutex::new(Vec::new())));
        assert_eq!(fetch_title(&invoke, "flash"), "flash");
    }

    // ── KIND proxy ──────────────────────────────────────────────────────────

    #[test]
    fn kind_proxy_title_accessor() {
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "Proxmox VM".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert_eq!(p.title(), "Proxmox VM");
    }

    #[test]
    fn kind_proxy_layout_uses_plugin_reply_when_present() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_LAYOUT, serde_json::json!(["node1", "vm", "100"]));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(r, seen.clone()),
        };
        assert_eq!(
            p.layout("100"),
            vec!["node1".to_string(), "vm".to_string(), "100".to_string()]
        );
        assert!(
            seen.lock().unwrap().iter().any(|s| s.contains("\"100\"")),
            "instance is forwarded in the layout args"
        );
    }

    #[test]
    fn kind_proxy_instances_surfaces_invalid_json() {
        // A reply that is not a Vec<String> must be a decode error, not silently
        // dropped.
        let mut r = std::collections::HashMap::new();
        r.insert(OP_INSTANCES, serde_json::json!({"not": "an array"}));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let err = p.instances().expect_err("invalid JSON must surface");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    #[tokio::test]
    async fn kind_proxy_backup_returns_outcome() {
        let mut r = std::collections::HashMap::new();
        r.insert(
            OP_BACKUP,
            serde_json::json!({"checksum": "abc123", "note": "full"}),
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(r, seen.clone()),
        };
        let out = p
            .backup(Path::new("/tmp/payload"), "100", &ctx())
            .await
            .unwrap();
        assert_eq!(out.checksum.as_deref(), Some("abc123"));
        assert_eq!(out.note.as_deref(), Some("full"));
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|s| s.contains("/tmp/payload")),
            "payload_dir crosses the seam as a path string"
        );
    }

    #[tokio::test]
    async fn kind_proxy_backup_surfaces_error() {
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        let err = p
            .backup(Path::new("/tmp/payload"), "100", &ctx())
            .await
            .expect_err("missing backup op must error");
        assert!(err.to_string().contains("backup"), "{err}");
    }

    #[tokio::test]
    async fn kind_proxy_restore_ignores_reply_body_on_success() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_RESTORE, serde_json::json!(null));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(r, seen.clone()),
        };
        p.restore(Path::new("/tmp/payload"), "100", &ctx())
            .await
            .expect("null reply body is fine for a unit op");
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|s| s.starts_with("restore:"))
        );
    }

    #[tokio::test]
    async fn kind_proxy_restore_surfaces_error() {
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(
            p.restore(Path::new("/tmp/payload"), "100", &ctx())
                .await
                .is_err()
        );
    }

    // ── TARGET proxy ────────────────────────────────────────────────────────

    #[test]
    fn target_proxy_kind_and_title_accessors() {
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "NFS Share".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert_eq!(p.kind(), "nfs");
        assert_eq!(p.title(), "NFS Share");
    }

    #[tokio::test]
    async fn target_proxy_open_surfaces_error() {
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p.open("default", &ctx()).await.is_err());
    }

    #[tokio::test]
    async fn target_proxy_sync_and_refresh_succeed() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_SYNC, serde_json::json!({}));
        r.insert(OP_REFRESH, serde_json::json!({}));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, seen.clone()),
        };
        p.sync("default", &ctx()).await.unwrap();
        p.refresh("default", &ctx()).await.unwrap();
        let log = seen.lock().unwrap();
        assert!(log.iter().any(|s| s.starts_with("sync:")));
        assert!(log.iter().any(|s| s.starts_with("refresh:")));
    }

    #[tokio::test]
    async fn target_proxy_sync_surfaces_error() {
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p.sync("default", &ctx()).await.is_err());
        assert!(p.refresh("default", &ctx()).await.is_err());
    }

    #[test]
    fn target_proxy_fits_honors_plugin_reply() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_FITS, serde_json::json!(false));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(!p.fits(&Placement::bare()), "explicit false is honored");

        let mut r2 = std::collections::HashMap::new();
        r2.insert(OP_FITS, serde_json::json!(true));
        let p2 = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r2, Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p2.fits(&Placement::bare()));
    }

    #[test]
    fn target_proxy_default_retention_some_none_and_error() {
        // Some(...) reply.
        let mut r = std::collections::HashMap::new();
        r.insert(OP_DEFAULT_RETENTION, serde_json::json!({"keep_last": 7}));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let ret = p.default_retention("default").expect("Some retention");
        assert_eq!(ret.keep_last, Some(7));

        // null reply → None.
        let mut r2 = std::collections::HashMap::new();
        r2.insert(OP_DEFAULT_RETENTION, serde_json::json!(null));
        let p2 = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r2, Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p2.default_retention("default").is_none());

        // Missing op → error path falls back to None.
        let p3 = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p3.default_retention("default").is_none());
    }

    #[test]
    fn target_proxy_default_schedule_some_and_error() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_DEFAULT_SCHEDULE, serde_json::json!("daily"));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        assert_eq!(p.default_schedule("default"), Some(BackupSchedule::Daily));

        // Missing op → error path falls back to None.
        let p2 = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p2.default_schedule("default").is_none());
    }

    #[tokio::test]
    async fn target_proxy_available_returns_locations() {
        let mut r = std::collections::HashMap::new();
        r.insert(
            OP_AVAILABLE,
            serde_json::json!([{
                "id": "backups",
                "label": "SMB //nas/backups",
                "basePath": "/mnt/nas/backups",
                "backingKey": "nfs://nas/backups"
            }]),
        );
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let locs = p.available(&ctx()).await.unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].id, "backups");
        assert_eq!(locs[0].backing_key, "nfs://nas/backups");
        assert_eq!(locs[0].base_path.as_deref(), Some("/mnt/nas/backups"));
    }

    #[tokio::test]
    async fn target_proxy_available_surfaces_error() {
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p.available(&ctx()).await.is_err());
    }

    #[tokio::test]
    async fn target_proxy_backing_key_returns_string_and_errors() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_BACKING_KEY, serde_json::json!("nfs://nas/backups"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, seen.clone()),
        };
        assert_eq!(
            p.backing_key("default", &ctx()).await.unwrap(),
            "nfs://nas/backups"
        );
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|s| s.starts_with("backing_key:"))
        );

        let p2 = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p2.backing_key("default", &ctx()).await.is_err());
    }

    // ── claim_owner unit tests (isolated registries) ────────────────────────

    #[test]
    fn claim_owner_records_and_allows_same_owner_reload() {
        let owners = RwLock::new(HashMap::new());
        claim_owner(&owners, "kind", "k1", "pluginA", false).expect("first claim");
        // Same owner re-claiming (reload) succeeds.
        claim_owner(&owners, "kind", "k1", "pluginA", false).expect("reload");
        assert_eq!(
            owners.read().unwrap().get("k1").map(String::as_str),
            Some("pluginA")
        );
    }

    #[test]
    fn claim_owner_rejects_different_owner() {
        let owners = RwLock::new(HashMap::new());
        claim_owner(&owners, "target", "k1", "pluginA", false).unwrap();
        let err = claim_owner(&owners, "target", "k1", "pluginB", false)
            .expect_err("different owner rejected");
        assert!(
            err.to_string().contains("already registered by plugin"),
            "{err}"
        );
    }

    #[test]
    fn claim_owner_rejects_unowned_when_builtin_present() {
        let owners = RwLock::new(HashMap::new());
        let err = claim_owner(&owners, "kind", "host", "evil", true)
            .expect_err("builtin collision rejected");
        assert!(err.to_string().contains("built-in"), "{err}");
        // Registry unchanged after a rejected claim.
        assert!(owners.read().unwrap().is_empty());
    }

    // ── target registration lifecycle ───────────────────────────────────────

    #[test]
    fn register_target_rejects_empty_kind() {
        let def = BackendDef {
            domain: DOMAIN_TARGET.to_string(),
            ..Default::default()
        };
        let err = register_target_from_def(
            &def,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect_err("empty kind rejected");
        assert!(err.to_string().contains("empty kind"), "{err}");
    }

    fn target_def(kind: &str, owner: &str) -> BackendDef {
        BackendDef {
            domain: DOMAIN_TARGET.to_string(),
            name: kind.to_string(),
            kind: kind.to_string(),
            invoke_prefix: owner.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn register_target_accepts_and_deregister_clears_owner() {
        let def = target_def("acc-target", "acc.target.plugin");
        register_target_from_def(
            &def,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect("name == kind accepted");
        assert!(target::target("acc-target").is_some());
        assert_eq!(
            TARGET_OWNERS
                .read()
                .unwrap()
                .get("acc-target")
                .map(String::as_str),
            Some("acc.target.plugin")
        );
        deregister_target_by_kind("acc-target");
        assert!(target::target("acc-target").is_none());
        assert!(!TARGET_OWNERS.read().unwrap().contains_key("acc-target"));
    }

    #[test]
    fn register_target_rejects_second_plugin_claiming_same_kind() {
        let a = target_def("duptgt", "tgtA.backup");
        register_target_from_def(
            &a,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect("first registration succeeds");

        let b = target_def("duptgt", "tgtB.backup");
        let err = register_target_from_def(
            &b,
            thunk(Default::default(), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect_err("second plugin on same kind rejected");
        assert!(
            err.to_string().contains("already registered by plugin"),
            "{err}"
        );

        deregister_target_by_kind("duptgt");
    }

    // ── async decode-error branches (call_async invalid-JSON path) ───────────
    //
    // The existing async error tests use a *missing* op, which trips the
    // invoke-failed branch. These feed a present-but-undeserializable reply so
    // the distinct "returned invalid JSON" decode branch of `call_async` runs.

    #[tokio::test]
    async fn kind_proxy_backup_surfaces_invalid_json() {
        let mut r = std::collections::HashMap::new();
        // A bare number cannot deserialize into BackupOutcome.
        r.insert(OP_BACKUP, serde_json::json!(42));
        let p = BackupKindProxy {
            kind: "vm".into(),
            title: "vm".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let err = p
            .backup(Path::new("/tmp/payload"), "100", &ctx())
            .await
            .expect_err("undeserializable reply must surface");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    #[tokio::test]
    async fn target_proxy_available_surfaces_invalid_json() {
        let mut r = std::collections::HashMap::new();
        // Not an array of locations.
        r.insert(OP_AVAILABLE, serde_json::json!({"nope": true}));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let err = p
            .available(&ctx())
            .await
            .expect_err("undeserializable reply must surface");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    #[tokio::test]
    async fn target_proxy_backing_key_surfaces_invalid_json() {
        let mut r = std::collections::HashMap::new();
        // A number cannot deserialize into String.
        r.insert(OP_BACKING_KEY, serde_json::json!(7));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let err = p
            .backing_key("default", &ctx())
            .await
            .expect_err("undeserializable reply must surface");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    #[tokio::test]
    async fn target_proxy_open_surfaces_invalid_reply_shape() {
        let mut r = std::collections::HashMap::new();
        // Missing the required `root` field → OpenReply decode fails.
        r.insert(OP_OPEN, serde_json::json!({"unexpected": "x"}));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        let err = p
            .open("default", &ctx())
            .await
            .expect_err("missing root must surface");
        assert!(err.to_string().contains("invalid JSON"), "{err}");
    }

    // ── target metadata: sync decode-error fallbacks ─────────────────────────

    #[test]
    fn target_proxy_default_retention_invalid_json_falls_back_to_none() {
        // A present but malformed reply exercises the call_sync decode-error arm
        // (distinct from the missing-op arm), which also falls back to None.
        let mut r = std::collections::HashMap::new();
        r.insert(OP_DEFAULT_RETENTION, serde_json::json!("not-a-retention"));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p.default_retention("default").is_none());
    }

    #[test]
    fn target_proxy_default_schedule_invalid_json_falls_back_to_none() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_DEFAULT_SCHEDULE, serde_json::json!({"bogus": 1}));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, Arc::new(Mutex::new(Vec::new()))),
        };
        assert!(p.default_schedule("default").is_none());
    }

    #[test]
    fn target_proxy_fits_forwards_placement_and_honors_true() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_FITS, serde_json::json!(true));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
            title: "nfs".into(),
            invoke: thunk(r, seen.clone()),
        };
        assert!(p.fits(&Placement::bare()));
        assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("fits:")));
    }

    // ── install() wires both domain constructors into the loader ──────────────

    #[test]
    fn install_registers_both_domain_constructors() {
        // install() is idempotent registration of the KIND + TARGET domain
        // constructors into the loader's extension table; calling it must not
        // panic and leaves the injected domains dispatchable.
        install();
    }

    #[test]
    fn register_kind_fetches_title_at_registration() {
        // A registered kind proxy exposes the plugin-supplied title over its
        // trait accessor, proving fetch_title ran during registration.
        let mut r = std::collections::HashMap::new();
        r.insert(OP_TITLE, serde_json::json!("Titled Kind"));
        let def = kind_def("titled-kind", "titled.plugin");
        register_kind_from_def(&def, thunk(r, Arc::new(Mutex::new(Vec::new()))))
            .expect("registration succeeds");
        let prov = provider::provider("titled-kind").expect("registered");
        assert_eq!(prov.title(), "Titled Kind");
        deregister_kind("titled-kind");
    }
}
