//! Backup provider trait + process-global registry.
//!
//! A [`BackupProvider`] is the "how" for one backup domain: it knows what a
//! `host` / `sonarr` / `nfs` backup captures and how to put it back. The generic
//! `backup.*` tool surface is the "what" — it owns dating, listing, selection,
//! and retention via [`super::store::BackupStore`] and drives whatever providers
//! are registered here. This mirrors the `service::ServiceBackend` registry
//! ([[always-hunt-abstractions-reuse-seams]]); plugins and core domains register
//! against it so `orca backup` fans out over every registered provider.
//!
//! No `async_trait` macro ([[no-async-trait-macro]]): async methods return the
//! hand-desugared [`contract::BoxFuture`], exactly as `ServiceBackend` does.

use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use contract::{BoxFuture, ToolCtx};

/// Provider-supplied metadata about a completed backup, folded into the
/// [`BackupRecord`](contract::backup::BackupRecord) the store writes.
#[derive(Debug, Default, Clone)]
pub struct BackupOutcome {
    /// Optional integrity checksum over the payload (provider-defined algorithm).
    pub checksum: Option<String>,
    /// Free-form note on what was captured (paths, strategy, …).
    pub note: Option<String>,
}

/// One backup KIND. The tool layer allocates a slot, hands the provider its
/// `payload_dir` to write into, then commits the slot on success (or aborts it
/// on error). `restore` is the inverse: given a previously produced payload dir,
/// put the state back.
pub trait BackupProvider: Send + Sync {
    /// Kind name (`"host"`, `"service"`, `"nfs"`). Unique across the registry;
    /// it is the `--kind` selector and the store path segment.
    fn kind(&self) -> &str;

    /// Human-facing title for listings. Defaults to the kind.
    fn title(&self) -> &str {
        self.kind()
    }

    /// Instances this provider can back up. Single-instance kinds keep the
    /// default `["default"]`; a multi-instance kind (several sonarr endpoints)
    /// overrides this.
    fn instances(&self) -> Vec<String> {
        vec!["default".to_string()]
    }

    /// The labeled layout segments this instance's backups are filed under,
    /// beneath a target's root — the `<category>/<class>/<name>` taxonomy that
    /// makes backups self-organizing and navigable on any backing (a host on
    /// Proxmox → `["hosts","proxmox","thor"]`; a docker service →
    /// `["containers","docker","sonarr"]`). The store treats the result as an
    /// opaque relative path; the manifest still carries the true `kind`/`instance`
    /// identity, so listing/selection are unaffected by the layout.
    ///
    /// Default: `[kind, instance]` — the flat, backward-compatible layout. A
    /// provider overrides this to declare its taxonomy. Segments are sanitized by
    /// the store, so a provider need not escape path separators itself.
    fn layout(&self, instance: &str) -> Vec<String> {
        vec![self.kind().to_string(), instance.to_string()]
    }

    /// Capture `instance`'s state into `payload_dir` (already created, empty).
    /// The provider writes files under it and returns metadata. Returning `Err`
    /// makes the tool layer abort the slot, leaving no partial backup.
    fn backup<'a>(
        &'a self,
        payload_dir: &'a Path,
        instance: &'a str,
        ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<BackupOutcome>>;

    /// Restore `instance` from a previously produced `payload_dir`.
    fn restore<'a>(
        &'a self,
        payload_dir: &'a Path,
        instance: &'a str,
        ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<()>>;
}

// ── Registry ─────────────────────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn BackupProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register (or replace, by kind name) a backup provider.
pub fn register_provider(provider: Arc<dyn BackupProvider>) {
    let mut g = GLOBAL.write().expect("backup provider registry poisoned");
    let kind = provider.kind().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.kind() == kind) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Every registered provider.
pub fn providers() -> Vec<Arc<dyn BackupProvider>> {
    GLOBAL
        .read()
        .expect("backup provider registry poisoned")
        .clone()
}

/// The provider for `kind`, if one is registered.
pub fn provider(kind: &str) -> Option<Arc<dyn BackupProvider>> {
    GLOBAL
        .read()
        .expect("backup provider registry poisoned")
        .iter()
        .find(|p| p.kind() == kind)
        .cloned()
}

/// Remove the provider for `kind`, so the surfaces stop offering it. Returns true
/// if one was removed.
pub fn deregister_provider(kind: &str) -> bool {
    let mut g = GLOBAL.write().expect("backup provider registry poisoned");
    let before = g.len();
    g.retain(|p| p.kind() != kind);
    before != g.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Fake {
        kind: String,
    }

    impl BackupProvider for Fake {
        fn kind(&self) -> &str {
            &self.kind
        }
        fn instances(&self) -> Vec<String> {
            vec!["a".into(), "b".into()]
        }
        fn backup<'a>(
            &'a self,
            payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> BoxFuture<'a, Result<BackupOutcome>> {
            Box::pin(async move {
                std::fs::write(payload_dir.join("marker"), b"x")?;
                Ok(BackupOutcome {
                    checksum: None,
                    note: Some("fake".into()),
                })
            })
        }
        fn restore<'a>(
            &'a self,
            _payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    #[test]
    fn register_lookup_replace_deregister() {
        let kind = "fake-test-provider";
        deregister_provider(kind);

        assert!(provider(kind).is_none());
        register_provider(Arc::new(Fake { kind: kind.into() }));
        let p = provider(kind).expect("registered");
        assert_eq!(p.kind(), kind);
        assert_eq!(p.instances(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(p.title(), kind, "title defaults to kind");

        // Re-register replaces, does not duplicate.
        let before = providers().iter().filter(|p| p.kind() == kind).count();
        register_provider(Arc::new(Fake { kind: kind.into() }));
        let after = providers().iter().filter(|p| p.kind() == kind).count();
        assert_eq!(before, 1);
        assert_eq!(after, 1);

        assert!(deregister_provider(kind));
        assert!(provider(kind).is_none());
    }
}
