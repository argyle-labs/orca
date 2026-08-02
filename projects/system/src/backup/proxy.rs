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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use contract::backup::{BackupSchedule, Placement, Retention};
use contract::{BoxFuture, ToolCtx};
use plugin_loader::BackendInvoke;
use plugin_toolkit::abi::BackendDef;
use serde::{Deserialize, Serialize};

use super::provider::{self, BackupOutcome, BackupProvider};
use super::store::BackupStore;
use super::target::{self, BackupTargetProvider, TargetLocation};

/// Domain string a plugin declares to contribute a backup KIND.
pub const DOMAIN_KIND: &str = "backup_kind";
/// Domain string a plugin declares to contribute a backup TARGET.
pub const DOMAIN_TARGET: &str = "backup_target";

// KIND ops.
const OP_INSTANCES: &str = "instances";
const OP_LAYOUT: &str = "layout";
const OP_BACKUP: &str = "backup";
const OP_RESTORE: &str = "restore";
// TARGET ops.
const OP_OPEN: &str = "open";
const OP_SYNC: &str = "sync";
const OP_REFRESH: &str = "refresh";
const OP_FITS: &str = "fits";
const OP_DEFAULT_RETENTION: &str = "default_retention";
const OP_DEFAULT_SCHEDULE: &str = "default_schedule";
const OP_AVAILABLE: &str = "available";
const OP_BACKING_KEY: &str = "backing_key";

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

// ── KIND proxy ────────────────────────────────────────────────────────────────

/// Build and register a [`BackupProvider`] from a plugin descriptor plus its
/// invoke thunk. Loader dispatch entry for `domain = "backup_kind"`; `def.kind`
/// is the KIND name (`"vm"`, `"lxc"`, `"flash"`).
pub fn register_kind_from_def(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    if def.kind.is_empty() {
        return Err(anyhow!("backup_kind backend '{}' has empty kind", def.name));
    }
    provider::register_provider(Arc::new(BackupKindProxy {
        kind: def.kind.clone(),
        invoke,
    }));
    Ok(())
}

fn deregister_kind(kind: &str) {
    provider::deregister_provider(kind);
}

#[derive(Serialize)]
struct InstanceArgs<'a> {
    instance: &'a str,
}

#[derive(Serialize)]
struct PayloadArgs<'a> {
    /// Host-local path the plugin subprocess reads/writes directly (shared fs).
    payload_dir: String,
    instance: &'a str,
}

/// A [`BackupProvider`] backed by a subprocess plugin over the JSON-proxy seam.
struct BackupKindProxy {
    kind: String,
    invoke: BackendInvoke,
}

impl BackupKindProxy {
    /// Synchronous metadata call — the loader thunk is itself synchronous, so a
    /// sync trait method (`instances`/`layout`) drives it directly. Must stay
    /// cheap on the plugin side: it is not offloaded to a blocking pool.
    fn call_sync<T: for<'de> Deserialize<'de>>(&self, op: &str, args_json: String) -> Result<T> {
        let out = (self.invoke)(op, args_json)
            .map_err(|e| anyhow!("backup kind '{}' {op} failed: {e}", self.kind))?;
        serde_json::from_str(&out).map_err(|e| {
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
        args_json: String,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
                .await
                .map_err(|e| anyhow!("backup kind '{kind}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow!("backup kind '{kind}' {op} failed: {e}"))?;
            serde_json::from_str(&out)
                .map_err(|e| anyhow!("backup kind '{kind}' {op} returned invalid JSON: {e}"))
        })
    }

    /// A `()`-returning async op: the plugin's reply body (`null` / `{}` / empty)
    /// is ignored — only success/failure matters.
    fn call_async_unit(&self, op: &'static str, args_json: String) -> BoxFuture<'_, Result<()>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || invoke(op, args_json))
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

    fn instances(&self) -> Vec<String> {
        match self.call_sync::<Vec<String>>(OP_INSTANCES, "{}".to_string()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("{e}; falling back to [\"default\"]");
                vec!["default".to_string()]
            }
        }
    }

    fn layout(&self, instance: &str) -> Vec<String> {
        let args = serde_json::to_string(&InstanceArgs { instance }).unwrap_or_default();
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
        let args = serde_json::to_string(&PayloadArgs {
            payload_dir: payload_dir.to_string_lossy().into_owned(),
            instance,
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
        let args = serde_json::to_string(&PayloadArgs {
            payload_dir: payload_dir.to_string_lossy().into_owned(),
            instance,
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
    target::register_target(Arc::new(BackupTargetProxy {
        kind: def.kind.clone(),
        invoke,
    }));
    Ok(())
}

fn deregister_target_by_kind(kind: &str) {
    target::deregister_target(kind);
}

/// A [`BackupTargetProvider`] backed by a subprocess plugin over the JSON-proxy
/// seam. `open` resolves to a host-local root path the plugin provisioned; the
/// generic [`BackupStore`] then owns layout/listing/retention beneath it.
struct BackupTargetProxy {
    kind: String,
    invoke: BackendInvoke,
}

#[derive(Serialize)]
struct NameArgs<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct FitsArgs<'a> {
    placement: &'a Placement,
}

#[derive(Deserialize)]
struct OpenReply {
    /// Host-local root path the plugin provisioned for this target instance.
    root: String,
}

impl BackupTargetProxy {
    fn call_sync<T: for<'de> Deserialize<'de>>(&self, op: &str, args_json: String) -> Result<T> {
        let out = (self.invoke)(op, args_json)
            .map_err(|e| anyhow!("backup target '{}' {op} failed: {e}", self.kind))?;
        serde_json::from_str(&out).map_err(|e| {
            anyhow!(
                "backup target '{}' {op} returned invalid JSON: {e}",
                self.kind
            )
        })
    }

    fn call_async<T: for<'de> Deserialize<'de> + Send + 'static>(
        &self,
        op: &'static str,
        args_json: String,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
                .await
                .map_err(|e| anyhow!("backup target '{kind}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow!("backup target '{kind}' {op} failed: {e}"))?;
            serde_json::from_str(&out)
                .map_err(|e| anyhow!("backup target '{kind}' {op} returned invalid JSON: {e}"))
        })
    }

    /// A `()`-returning async op: the plugin's reply body is ignored.
    fn call_async_unit(&self, op: &'static str, args_json: String) -> BoxFuture<'_, Result<()>> {
        let invoke = self.invoke.clone();
        let kind = self.kind.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || invoke(op, args_json))
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

    fn open<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<BackupStore>> {
        let args = serde_json::to_string(&NameArgs { name }).unwrap_or_default();
        Box::pin(async move {
            let reply: OpenReply = self.call_async(OP_OPEN, args).await?;
            Ok(BackupStore::new(PathBuf::from(reply.root)))
        })
    }

    fn sync<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<()>> {
        let args = serde_json::to_string(&NameArgs { name }).unwrap_or_default();
        self.call_async_unit(OP_SYNC, args)
    }

    fn refresh<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<()>> {
        let args = serde_json::to_string(&NameArgs { name }).unwrap_or_default();
        self.call_async_unit(OP_REFRESH, args)
    }

    fn fits(&self, placement: &Placement) -> bool {
        let args = serde_json::to_string(&FitsArgs { placement }).unwrap_or_default();
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
        let args = serde_json::to_string(&NameArgs { name }).unwrap_or_default();
        self.call_sync::<Option<Retention>>(OP_DEFAULT_RETENTION, args)
            .unwrap_or_else(|e| {
                tracing::warn!("{e}; falling back to policy default");
                None
            })
    }

    fn default_schedule(&self, name: &str) -> Option<BackupSchedule> {
        let args = serde_json::to_string(&NameArgs { name }).unwrap_or_default();
        self.call_sync::<Option<BackupSchedule>>(OP_DEFAULT_SCHEDULE, args)
            .unwrap_or_else(|e| {
                tracing::warn!("{e}; falling back to policy default");
                None
            })
    }

    fn available<'a>(&'a self, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Vec<TargetLocation>>> {
        self.call_async(OP_AVAILABLE, "{}".to_string())
    }

    fn backing_key<'a>(
        &'a self,
        name: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<String>> {
        let args = serde_json::to_string(&NameArgs { name }).unwrap_or_default();
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
        responses: std::collections::HashMap<&'static str, String>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> BackendInvoke {
        Arc::new(move |op: &str, args: String| {
            seen.lock().unwrap().push(format!("{op}:{args}"));
            responses
                .get(op)
                .cloned()
                .ok_or_else(|| format!("no canned response for {op}"))
        })
    }

    #[test]
    fn kind_proxy_metadata_and_fallbacks() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_INSTANCES, r#"["a","b"]"#.to_string());
        // No layout response → must fall back to [kind, instance].
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            invoke: thunk(r, seen),
        };
        assert_eq!(p.kind(), "vm");
        assert_eq!(p.instances(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            p.layout("100"),
            vec!["vm".to_string(), "100".to_string()],
            "missing layout op falls back to flat layout"
        );
    }

    #[test]
    fn kind_proxy_instances_fallback_on_error() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupKindProxy {
            kind: "vm".into(),
            invoke: thunk(std::collections::HashMap::new(), seen),
        };
        assert_eq!(p.instances(), vec!["default".to_string()]);
    }

    #[tokio::test]
    async fn target_proxy_open_wraps_returned_root() {
        let mut r = std::collections::HashMap::new();
        r.insert(OP_OPEN, r#"{"root":"/mnt/nas/backups"}"#.to_string());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = BackupTargetProxy {
            kind: "nfs".into(),
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
}
