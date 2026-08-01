//! Generic backup/restore tool surface — a single `backup` domain, parameterized
//! by `--kind` (mirroring how `service.*` is generic-over-service):
//!
//! * `backup.providers` — every registered backup kind + its instances.
//! * `backup.list`      — dated backups (all, or narrowed by kind/instance).
//! * `backup.run`       — run one kind, or ALL when `--kind` is omitted (this is
//!   `orca backup`). Fans out log-and-skip, per [[fail-loud-logging-levels]].
//! * `backup.restore`   — date-selected restore with surface-safe gating.
//!
//! Kinds are entries in the [`provider`] registry (host, service, …) — there is
//! ONE backup system, not a per-kind verb surface. The store owns dating,
//! listing, selection, and retention.
//!
//! Restore is destructive and `ToolCtx` carries no surface (CLI/MCP/REST) signal,
//! so safety is enforced with an explicit arg, the `diagnostics.repair { confirm }`
//! pattern: with neither `--id <id>` nor `--approve-all`, restore does NOT run —
//! it returns the available backups and asks for a selection. This makes MCP/REST
//! require an explicit id (and be able to list first) for free.
//!
//! Dispatched through the single daemon handler so CLI / REST / MCP / UI share
//! one path ([[feedback-cli-api-mcp-one-path]]).

use std::path::PathBuf;
use std::sync::Arc;

use contract::ToolCtx;
use contract::backup::{BackupRecord, BackupSelector, Retention};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::host::HostBackupProvider;
use super::provider::{self, BackupProvider};
use super::service_kind::ServiceKindProvider;
use super::store::BackupStore;

const DEFAULT_INSTANCE: &str = "default";

// ── providers ─────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    /// The `--kind` selector (`host`, `service`, …).
    pub kind: String,
    pub title: String,
    pub instances: Vec<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProvidersArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersOutput {
    pub providers: Vec<ProviderInfo>,
}

/// Every backup kind registered with this daemon, with the instances each
/// advertises. Empty before any provider registers.
#[orca_tool(domain = "backup", verb = "providers")]
async fn backup_providers(_args: ProvidersArgs, _ctx: &ToolCtx) -> anyhow::Result<ProvidersOutput> {
    let providers = provider::providers()
        .into_iter()
        .map(|p| ProviderInfo {
            kind: p.kind().to_string(),
            title: p.title().to_string(),
            instances: p.instances(),
        })
        .collect();
    Ok(ProvidersOutput { providers })
}

// ── list ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupListArgs {
    /// Restrict to one kind (e.g. `host`). Omit for every kind.
    #[arg(long)]
    pub kind: Option<String>,
    /// Restrict to one instance within the kind. Omit for every instance.
    #[arg(long)]
    pub instance: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupListOutput {
    /// Matching backups, newest first.
    pub backups: Vec<BackupRecord>,
}

/// List available backups, newest first — the set a restore selects from.
#[orca_tool(domain = "backup", verb = "list")]
async fn backup_list(args: BackupListArgs, _ctx: &ToolCtx) -> anyhow::Result<BackupListOutput> {
    let store = BackupStore::default_store()?;
    let backups = store.list(args.kind.as_deref(), args.instance.as_deref())?;
    Ok(BackupListOutput { backups })
}

// ── run ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupError {
    pub kind: String,
    pub instance: String,
    pub error: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupRunOutput {
    /// Records produced this run.
    pub produced: Vec<BackupRecord>,
    /// Per-(kind,instance) failures — the run does not abort on one failure.
    pub errors: Vec<BackupError>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupRunArgs {
    /// Kind to back up (e.g. `host`). Omit to back up EVERY registered kind —
    /// this is `orca backup`.
    #[arg(long)]
    pub kind: Option<String>,
    /// Instance to back up. Omit for every instance the kind advertises.
    #[arg(long)]
    pub instance: Option<String>,
}

/// Run backups. With `--kind` it backs up that kind; without, it fans out over
/// every registered kind (log-and-skip on failure). Old backups beyond the
/// retention policy are pruned per instance.
#[orca_tool(domain = "backup", verb = "run", data_mutation = true, role = "admin")]
async fn backup_run(args: BackupRunArgs, ctx: &ToolCtx) -> anyhow::Result<BackupRunOutput> {
    let store = BackupStore::default_store()?;
    let targets = resolve_providers(args.kind.as_deref())?;
    Ok(run_backups(&store, &targets, args.instance.as_deref(), ctx).await)
}

// ── restore ───────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupRestoreArgs {
    /// Kind to restore (e.g. `host`).
    #[arg(long)]
    pub kind: String,
    /// Instance to restore. Defaults to `default`.
    #[arg(long)]
    pub instance: Option<String>,
    /// The backup id to restore (from `backup.list`), or `latest`. REQUIRED for
    /// MCP/REST; on the CLI you may instead pass `--approve-all`.
    #[arg(long)]
    pub id: Option<String>,
    /// Restore the latest backup without naming an id. The explicit
    /// acknowledgement that makes a no-`--id` restore run.
    #[arg(long, default_value_t = false)]
    pub approve_all: bool,
}

/// The outcome of a restore call: either it ran, or it refused pending a
/// selection and returned the choices.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum BackupRestoreOutput {
    /// No `id`/`approve_all` was given: nothing was restored; pick from `available`.
    AwaitingSelection {
        message: String,
        available: Vec<BackupRecord>,
    },
    /// The restore ran from `record`.
    Restored { record: BackupRecord },
}

/// Restore a kind/instance from a dated backup. Destructive: without `--id` or
/// `--approve-all` it lists the available backups and restores nothing.
#[orca_tool(
    domain = "backup",
    verb = "restore",
    data_mutation = true,
    role = "admin"
)]
async fn backup_restore(
    args: BackupRestoreArgs,
    ctx: &ToolCtx,
) -> anyhow::Result<BackupRestoreOutput> {
    let instance = args.instance.as_deref().unwrap_or(DEFAULT_INSTANCE);
    restore_one(
        &args.kind,
        instance,
        args.id.as_deref(),
        args.approve_all,
        ctx,
    )
    .await
}

// ── shared machinery ──────────────────────────────────────────────────

/// The providers a run/restore targets: one named kind (erroring if unknown), or
/// all registered providers when `kind` is `None`.
fn resolve_providers(kind: Option<&str>) -> anyhow::Result<Vec<Arc<dyn BackupProvider>>> {
    match kind {
        Some(k) => {
            let p = provider::provider(k)
                .ok_or_else(|| anyhow::anyhow!("no backup provider for kind `{k}`"))?;
            Ok(vec![p])
        }
        None => Ok(provider::providers()),
    }
}

/// Back up each provider (optionally narrowed to one instance), committing each
/// slot and pruning per the default retention. Failures are collected, never
/// fatal — a broken provider must not stop the rest.
async fn run_backups(
    store: &BackupStore,
    providers: &[Arc<dyn BackupProvider>],
    instance_filter: Option<&str>,
    ctx: &ToolCtx,
) -> BackupRunOutput {
    let mut out = BackupRunOutput::default();
    for p in providers {
        let instances: Vec<String> = match instance_filter {
            Some(i) => vec![i.to_string()],
            None => p.instances(),
        };
        for instance in instances {
            run_one(store, p, &instance, ctx, &mut out).await;
        }
    }
    out
}

/// Back up a single (provider, instance): allocate a slot, let the provider write
/// it, commit or abort, then prune old backups.
async fn run_one(
    store: &BackupStore,
    p: &Arc<dyn BackupProvider>,
    instance: &str,
    ctx: &ToolCtx,
    out: &mut BackupRunOutput,
) {
    let kind = p.kind();
    let slot = match store.new_slot(kind, instance) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[backup] {kind}/{instance}: cannot allocate slot: {e:#}");
            out.errors.push(BackupError {
                kind: kind.to_string(),
                instance: instance.to_string(),
                error: format!("{e:#}"),
            });
            return;
        }
    };
    // Release the borrow on `slot` before we consume it in commit/abort.
    let payload: PathBuf = slot.payload_dir().to_path_buf();
    match p.backup(&payload, instance, ctx).await {
        Ok(outcome) => match slot.commit(outcome.checksum, outcome.note) {
            Ok(rec) => out.produced.push(rec),
            Err(e) => {
                tracing::warn!("[backup] {kind}/{instance}: commit failed: {e:#}");
                out.errors.push(BackupError {
                    kind: kind.to_string(),
                    instance: instance.to_string(),
                    error: format!("{e:#}"),
                });
            }
        },
        Err(e) => {
            tracing::warn!("[backup] {kind}/{instance}: backup failed: {e:#}");
            if let Err(abort_err) = slot.abort() {
                tracing::warn!("[backup] {kind}/{instance}: slot cleanup failed: {abort_err:#}");
            }
            out.errors.push(BackupError {
                kind: kind.to_string(),
                instance: instance.to_string(),
                error: format!("{e:#}"),
            });
        }
    }
    if let Err(e) = store.prune(kind, instance, &Retention::default()) {
        tracing::warn!("[backup] {kind}/{instance}: prune failed: {e:#}");
    }
}

/// Restore one (kind, instance) with the surface-safe selection gate.
async fn restore_one(
    kind: &str,
    instance: &str,
    id: Option<&str>,
    approve_all: bool,
    ctx: &ToolCtx,
) -> anyhow::Result<BackupRestoreOutput> {
    let store = BackupStore::default_store()?;
    let p = provider::provider(kind)
        .ok_or_else(|| anyhow::anyhow!("no backup provider for kind `{kind}`"))?;

    let selector = match (id, approve_all) {
        (Some(i), _) => BackupSelector::parse(i),
        (None, true) => BackupSelector::Latest,
        (None, false) => {
            // Refuse: list the choices instead of restoring blind.
            let available = store.list(Some(kind), Some(instance))?;
            return Ok(BackupRestoreOutput::AwaitingSelection {
                message: format!(
                    "restore of {kind}/{instance} needs a selection: pass --id <id> \
                     (from the list) or --approve-all to restore the latest"
                ),
                available,
            });
        }
    };

    let record = store.resolve(kind, instance, &selector)?;
    let payload = PathBuf::from(&record.path);
    p.restore(&payload, instance, ctx)
        .await
        .map_err(|e| anyhow::anyhow!("restore {kind}/{instance} from {}: {e:#}", record.id))?;
    Ok(BackupRestoreOutput::Restored { record })
}

/// Register the built-in (core-owned) backup kinds. Called once at daemon
/// startup, alongside service-backend registration.
pub fn register_builtin_providers() {
    provider::register_provider(Arc::new(HostBackupProvider::new()));
    provider::register_provider(Arc::new(ServiceKindProvider::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::{Config, Model};
    use std::path::Path;

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

    /// A provider that writes one fixed file and checks the payload on restore.
    struct StubProvider {
        kind: String,
    }
    impl BackupProvider for StubProvider {
        fn kind(&self) -> &str {
            &self.kind
        }
        fn instances(&self) -> Vec<String> {
            vec!["default".into()]
        }
        fn backup<'a>(
            &'a self,
            payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<super::super::provider::BackupOutcome>>
        {
            Box::pin(async move {
                std::fs::write(payload_dir.join("data.txt"), b"stub")?;
                Ok(super::super::provider::BackupOutcome {
                    checksum: None,
                    note: Some("stub".into()),
                })
            })
        }
        fn restore<'a>(
            &'a self,
            payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                assert!(payload_dir.join("data.txt").exists());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn run_then_list_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> = vec![Arc::new(StubProvider {
            kind: "stub".into(),
        })];
        let ctx = ctx();

        let out = run_backups(&store, &providers, None, &ctx).await;
        assert_eq!(out.produced.len(), 1);
        assert!(out.errors.is_empty());
        let id = out.produced[0].id.clone();
        assert_eq!(out.produced[0].kind, "stub");

        let listed = store.list(Some("stub"), Some("default")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
    }

    #[tokio::test]
    async fn restore_without_selection_refuses_and_lists() {
        let p: Arc<dyn BackupProvider> = Arc::new(StubProvider {
            kind: "stub2".into(),
        });
        provider::register_provider(p.clone());

        // restore_one uses the DEFAULT store, so this asserts the *gate* logic
        // (no id, no approve_all): it must return AwaitingSelection, never Restored.
        let ctx = ctx();
        let res = restore_one("stub2", "default", None, false, &ctx)
            .await
            .unwrap();
        match res {
            BackupRestoreOutput::AwaitingSelection { message, .. } => {
                assert!(message.contains("--approve-all"));
                assert!(message.contains("--id"));
            }
            BackupRestoreOutput::Restored { .. } => panic!("must not restore without selection"),
        }
        provider::deregister_provider("stub2");
    }

    #[tokio::test]
    async fn run_unknown_kind_errors() {
        assert!(resolve_providers(Some("no-such-kind-xyz")).is_err());
    }

    #[test]
    fn builtin_providers_register_host_and_service() {
        register_builtin_providers();
        assert!(provider::provider("host").is_some());
        assert!(provider::provider("service").is_some());
    }

    #[test]
    fn restore_output_tags_are_stable() {
        let awaiting = BackupRestoreOutput::AwaitingSelection {
            message: "m".into(),
            available: vec![],
        };
        let v = serde_json::to_value(&awaiting).unwrap();
        assert_eq!(v["status"], "awaitingSelection");

        let restored = BackupRestoreOutput::Restored {
            record: BackupRecord {
                id: "20260101-000000".into(),
                kind: "host".into(),
                instance: "default".into(),
                created_ms: 1,
                path: "/p".into(),
                size_bytes: 0,
                file_count: 0,
                checksum: None,
                note: None,
            },
        };
        let v = serde_json::to_value(&restored).unwrap();
        assert_eq!(v["status"], "restored");
        assert_eq!(v["record"]["kind"], "host");
    }
}
