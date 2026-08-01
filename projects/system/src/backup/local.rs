//! The built-in `local` backup target — the ONLY target kind core owns
//! ([[orca-core-generic-plugins-expose-functionality]]). A plain directory on
//! the host filesystem; always available, needs no network, and is the fallback
//! when no plugin target is configured.
//!
//! Config-driven ([[orca-must-be-declarative-config-driven]]): a target instance
//! `name` reads its root path from the `backup`/`target:local:<name>` config row.
//! With no row (or an empty path) it resolves to the default store
//! (`~/.orca/backups`), so a bare host always has a usable local target.
//! `sync`/`refresh` are no-ops — a local path is already durable.

use anyhow::{Context, Result};
use contract::{BoxFuture, ToolCtx};
use serde::{Deserialize, Serialize};

use super::store::BackupStore;
use super::target::{BackupTargetProvider, TargetLocation};

/// The `backup`/`target:local:<name>` config row shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalTargetConfig {
    /// Absolute root directory for this target's backups. Absent/empty → the
    /// default store (`~/.orca/backups`).
    #[serde(default)]
    pub path: Option<String>,
}

/// The `local` target provider.
#[derive(Debug, Default)]
pub struct LocalTarget;

impl LocalTarget {
    pub fn new() -> Self {
        Self
    }

    /// The config-row name for a local target instance: `target:local:<name>`.
    fn row_name(name: &str) -> String {
        format!("target:local:{name}")
    }
}

impl BackupTargetProvider for LocalTarget {
    fn kind(&self) -> &str {
        "local"
    }

    fn title(&self) -> &str {
        "Local filesystem"
    }

    fn open<'a>(&'a self, name: &'a str, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<BackupStore>> {
        Box::pin(async move {
            let cfg = load_config(name);
            match cfg.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                Some(path) => {
                    std::fs::create_dir_all(path)
                        .with_context(|| format!("create local backup target dir {path}"))?;
                    Ok(BackupStore::new(path))
                }
                None => BackupStore::default_store(),
            }
        })
    }

    /// The one local location: this host's default backups dir. A user can still
    /// point a `local` target at any path via its config row; this is the
    /// discoverable default.
    fn available<'a>(&'a self, _ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Vec<TargetLocation>>> {
        Box::pin(async move {
            let base = BackupStore::default_store()?
                .root()
                .to_string_lossy()
                .into_owned();
            Ok(vec![TargetLocation {
                id: "default".to_string(),
                label: format!("Local filesystem ({base})"),
                base_path: Some(base),
                backing_key: local_backing_key(),
            }])
        })
    }

    /// `local://<hostname>` — a per-host key, so two machines' local disks NEVER
    /// register as a fleet-wide collision even when their paths string-match.
    fn backing_key<'a>(
        &'a self,
        _name: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok(local_backing_key()) })
    }
}

/// Stable per-host backing id for local storage.
fn local_backing_key() -> String {
    format!(
        "local://{}",
        crate::host_identity::cli_hostname_or_fallback()
    )
}

/// Load a local target's config row, falling back to defaults on any DB/parse
/// error so a local target never fails to open just because config is
/// unavailable — it degrades to the default store.
fn load_config(name: &str) -> LocalTargetConfig {
    let row_name = LocalTarget::row_name(name);
    let read =
        db::pool::with_pooled_or_open(|conn| db::config_store::get(conn, "backup", &row_name));
    match read {
        Ok(Some(row)) => serde_json::from_str::<LocalTargetConfig>(&row.json).unwrap_or_else(|e| {
            tracing::warn!("[backup:local] bad backup/{row_name} config, using default store: {e}");
            LocalTargetConfig::default()
        }),
        Ok(None) => LocalTargetConfig::default(),
        Err(e) => {
            tracing::warn!(
                "[backup:local] cannot read backup/{row_name} config, using default store: {e}"
            );
            LocalTargetConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_title() {
        let t = LocalTarget::new();
        assert_eq!(t.kind(), "local");
        assert_eq!(t.title(), "Local filesystem");
    }

    #[test]
    fn row_name_is_namespaced_by_kind() {
        assert_eq!(LocalTarget::row_name("default"), "target:local:default");
        assert_eq!(LocalTarget::row_name("cold"), "target:local:cold");
    }

    #[test]
    fn config_parses_path_and_defaults() {
        let none: LocalTargetConfig = serde_json::from_str("{}").unwrap();
        assert!(none.path.is_none());
        let some: LocalTargetConfig = serde_json::from_str(r#"{"path":"/mnt/backups"}"#).unwrap();
        assert_eq!(some.path.as_deref(), Some("/mnt/backups"));
    }

    #[test]
    fn store_roots_at_the_given_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("target-root");
        let store = BackupStore::new(&root);
        assert_eq!(store.root(), root.as_path());
    }
}
