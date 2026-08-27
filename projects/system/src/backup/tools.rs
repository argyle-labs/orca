//! Generic backup/restore tool surface — a single `backup` domain, parameterized
//! by `--kind` (mirroring how `service.*` is generic-over-service):
//!
//! * `backup.detail{view=providers}` — every registered backup kind + its instances.
//! * `backup.detail{view=targets}`   — registered target kinds, placement fit, and
//!   the concrete locations each exposes for selection.
//! * `backup.list`      — dated backups (all, or narrowed by kind/instance).
//! * `backup.run`       — run one `--kind`, or every kind with `--all` (opt-in;
//!   neither refuses and lists the kinds). This is `orca backup`. Fans out
//!   log-and-skip, per [[fail-loud-logging-levels]]. Writes each backup under a
//!   provider-declared `category/class/name` layout beneath every configured
//!   target root, then checks the fleet for collisions.
//! * `backup.restore`   — date-selected restore with surface-safe gating.
//! * `backup.check`     — fleet-wide same-folder collision detection; raises a
//!   dismissable notification per collision ([[dismissable-notifications-subsystem]]).
//!
//! Kinds are entries in the [`provider`] registry (host, service, …) — there is
//! ONE backup system, not a per-kind verb surface. The store owns dating,
//! listing, selection, and retention.
//!
//! Restore is destructive and gated by an explicit arg: it runs only when given
//! `--id <id>` (a specific backup) or `--approve-all` (the latest). Given
//! neither, it returns the available backups and asks for a selection, which
//! requires MCP/REST callers to name an id and lets them list first.
//!
//! Dispatched through the single daemon handler so CLI / REST / MCP / UI share
//! one path ([[feedback-cli-api-mcp-one-path]]).

use std::path::PathBuf;
use std::sync::Arc;

use contract::ToolCtx;
use contract::backup::{BackupRecord, BackupSelector, BackupTargetRef, Placement, Retention};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::collision::{self, Destination, OwnedDestination};
use super::host::HostBackupProvider;
use super::local::LocalTarget;
use super::provider::{self, BackupProvider};
use super::service_kind::ServiceKindProvider;
use super::store::BackupStore;
use super::target::{self, TargetLocation};

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

/// Which facet `backup.detail` reports. `providers` = registered backup kinds +
/// instances; `targets` = registered target kinds, placement fit, locations.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum BackupDetailView {
    #[default]
    Providers,
    Targets,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupDetailArgs {
    /// Which facet to report. Defaults to `providers`.
    #[arg(long, value_enum, default_value = "providers")]
    #[serde(default)]
    pub view: BackupDetailView,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersOutput {
    pub providers: Vec<ProviderInfo>,
}

/// `backup.detail` payload — one variant per `view`.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(untagged)]
pub enum BackupDetailOutput {
    Providers(ProvidersOutput),
    Targets(TargetsOutput),
}

/// Read-only backup detail. `view=providers` lists every backup kind registered
/// with this daemon and the instances each advertises (empty before any provider
/// registers); `view=targets` lists every registered target kind, which fit the
/// current placement, and the targets backups currently fan out to.
#[orca_tool(domain = "backup", verb = "detail")]
async fn backup_detail(
    args: BackupDetailArgs,
    ctx: &ToolCtx,
) -> anyhow::Result<BackupDetailOutput> {
    match args.view {
        BackupDetailView::Providers => Ok(BackupDetailOutput::Providers(backup_providers().await)),
        BackupDetailView::Targets => Ok(BackupDetailOutput::Targets(backup_targets(ctx).await?)),
    }
}

async fn backup_providers() -> ProvidersOutput {
    let providers = provider::providers()
        .into_iter()
        .map(|p| ProviderInfo {
            kind: p.kind().to_string(),
            title: p.title().to_string(),
            // Listing is tolerant: a provider whose enumeration momentarily
            // fails shows no instances rather than failing the whole surface.
            instances: p.instances().unwrap_or_else(|e| {
                tracing::warn!("[backup] providers: enumerate {}: {e:#}", p.kind());
                Vec::new()
            }),
        })
        .collect();
    ProvidersOutput { providers }
}

// ── targets ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    /// The target KIND (`local`, or a plugin's `nfs`/`s3`/…).
    pub kind: String,
    pub title: String,
    /// Whether this kind is eligible for the detected placement (Proxmox → PBS).
    pub fits_here: bool,
    /// True for the core-owned built-in `local` target.
    pub builtin: bool,
    /// The concrete storage locations this kind exposes for selection (mounts,
    /// buckets). Empty if the kind advertises none.
    pub locations: Vec<TargetLocation>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TargetsOutput {
    /// Every registered target kind, with placement eligibility.
    pub registered: Vec<TargetInfo>,
    /// The targets `backup.run` currently fans out to (the `backup`/`targets`
    /// config, or the built-in `local` fallback).
    pub configured: Vec<BackupTargetRef>,
    /// The detected placement the eligibility was computed against.
    pub placement: Placement,
}

async fn backup_targets(ctx: &ToolCtx) -> anyhow::Result<TargetsOutput> {
    let placement = target::placement();
    let mut registered = Vec::new();
    for t in target::targets() {
        let locations = t.available(ctx).await.unwrap_or_else(|e| {
            tracing::warn!("[backup] target {} available() failed: {e:#}", t.kind());
            Vec::new()
        });
        registered.push(TargetInfo {
            kind: t.kind().to_string(),
            title: t.title().to_string(),
            fits_here: t.fits(&placement),
            builtin: t.kind() == "local",
            locations,
        });
    }
    Ok(TargetsOutput {
        registered,
        configured: configured_target_refs(),
        placement,
    })
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
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupListOutput {
    /// Matching backups, newest first.
    pub backups: Vec<BackupRecord>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total backups across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// List available backups, newest first — the set a restore selects from.
/// Aggregates across every configured target (refreshing each first).
#[orca_tool(domain = "backup", verb = "list")]
async fn backup_list(args: BackupListArgs, ctx: &ToolCtx) -> anyhow::Result<BackupListOutput> {
    let mut backups = Vec::new();
    for (r, store) in open_configured_targets(ctx, true).await {
        match store.list(args.kind.as_deref(), args.instance.as_deref()) {
            Ok(mut recs) => backups.append(&mut recs),
            Err(e) => tracing::warn!("[backup] list on target {}/{}: {e:#}", r.kind, r.name),
        }
    }
    // Newest first across all targets; the id stamp sorts chronologically.
    backups.sort_by(|a, b| b.id.cmp(&a.id));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(backups, &params);
    Ok(BackupListOutput {
        backups: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

// ── run ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupError {
    pub kind: String,
    pub instance: String,
    /// The target the failure occurred against (`<kind>/<name>`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub error: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupRunOutput {
    /// Records produced this run (across every target fanned out to).
    pub produced: Vec<BackupRecord>,
    /// Targets this run wrote to (`<kind>/<name>`).
    pub targets: Vec<String>,
    /// Per-(target,kind,instance) failures — the run does not abort on one.
    pub errors: Vec<BackupError>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupRunArgs {
    /// Kind to back up (e.g. `host`).
    #[arg(long)]
    pub kind: Option<String>,
    /// Instance to back up. Omit for every instance the kind advertises.
    #[arg(long)]
    pub instance: Option<String>,
    /// Back up EVERY registered kind. Opt-in: with neither `--kind` nor `--all`,
    /// the run refuses and lists the kinds so a caller chooses explicitly.
    #[arg(long)]
    #[serde(default)]
    pub all: bool,
}

/// Run backups. `--kind` backs up that kind; `--all` fans out over every
/// registered kind (log-and-skip on failure). All-kinds is opt-in: with neither,
/// the run refuses and lists the kinds so the caller chooses explicitly. Backups
/// are written to EVERY configured target (the `backup`/`targets` config, or the
/// built-in `local` fallback); old backups beyond the retention policy are pruned
/// per instance, per target.
#[orca_tool(domain = "backup", verb = "run", data_mutation = true, role = "admin")]
async fn backup_run(args: BackupRunArgs, ctx: &ToolCtx) -> anyhow::Result<BackupRunOutput> {
    let providers = resolve_run_providers(args.kind.as_deref(), args.all)?;
    if args.kind.is_none() && args.all {
        tracing::warn!("[backup] --all: backing up every registered kind");
    }
    let mut out = BackupRunOutput::default();

    for (r, store) in open_configured_targets(ctx, false).await {
        let label = format!("{}/{}", r.kind, r.name);
        // Resolve THIS target's retention once (its declared default, else the
        // built-in) and apply it when pruning each committed backup below.
        let retention = resolve_target_retention(&r);
        let mut sub = run_backups(
            &store,
            &providers,
            args.instance.as_deref(),
            &retention,
            ctx,
        )
        .await;
        // Tag this target's failures so a fan-out failure is attributable.
        for e in &mut sub.errors {
            e.target.get_or_insert_with(|| label.clone());
        }
        let wrote_something = !sub.produced.is_empty();
        out.produced.append(&mut sub.produced);
        out.errors.append(&mut sub.errors);

        // Reconcile the remote backing (git push / s3 upload) after committing.
        if wrote_something
            && let Some(tp) = target::target(&r.kind)
            && let Err(e) = tp.sync(&r.name, ctx).await
        {
            tracing::warn!("[backup] target {label} sync failed: {e:#}");
            out.errors.push(BackupError {
                kind: r.kind.clone(),
                instance: String::new(),
                target: Some(label.clone()),
                error: format!("sync: {e:#}"),
            });
        }
        out.targets.push(label);
    }

    // Self-report destinations and check the fleet for same-folder collisions.
    // Best-effort: a check failure must never fail the backup that succeeded.
    if let Err(e) = refresh_and_check_collisions(ctx).await {
        tracing::warn!("[backup] collision check failed: {e:#}");
    }
    Ok(out)
}

// ── check (fleet-wide collisions) ──────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupCheckArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CollisionInfo {
    pub backing_key: String,
    pub party_a: String,
    pub party_b: String,
    pub path_a: String,
    pub path_b: String,
    /// True when one path nests under the other (vs an exact same-folder clash).
    pub nested: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupCheckOutput {
    /// Fleet-wide same-folder collisions detected (empty = all clear).
    pub collisions: Vec<CollisionInfo>,
}

/// Check the whole fleet for backup destinations that write the same folder on
/// the same backing (which corrupts backups). Re-publishes this host's resolved
/// destinations, unions every node's, and raises a dismissable notification per
/// collision — non-blocking, "try to correct." Also clears notifications for
/// collisions that no longer exist.
#[orca_tool(
    domain = "backup",
    verb = "check",
    data_mutation = true,
    role = "admin"
)]
async fn backup_check(_args: BackupCheckArgs, ctx: &ToolCtx) -> anyhow::Result<BackupCheckOutput> {
    let collisions = refresh_and_check_collisions(ctx).await?;
    Ok(BackupCheckOutput {
        collisions: collisions
            .into_iter()
            .map(|c| CollisionInfo {
                backing_key: c.backing_key,
                party_a: c.party_a,
                party_b: c.party_b,
                path_a: c.path_a,
                path_b: c.path_b,
                nested: c.nested,
            })
            .collect(),
    })
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

/// The providers a `backup.run` targets, enforcing that all-kinds is opt-in: a
/// named `kind` resolves that one; `all` resolves every registered kind; neither
/// refuses and lists the registered kinds so the caller chooses explicitly.
fn resolve_run_providers(
    kind: Option<&str>,
    all: bool,
) -> anyhow::Result<Vec<Arc<dyn BackupProvider>>> {
    match (kind, all) {
        (Some(k), _) => resolve_providers(Some(k)),
        (None, true) => Ok(provider::providers()),
        (None, false) => {
            let kinds = provider::providers()
                .iter()
                .map(|p| p.kind().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "specify --kind <kind> to back up one, or --all to back up every \
                 registered kind ({kinds})"
            ))
        }
    }
}

/// The retention a target resolves to: its declared `default_retention` (the
/// storage tier), else the built-in default. There is no per-backup override on
/// a manual `backup run`, so the backup tier is `None`.
fn resolve_target_retention(r: &contract::backup::BackupTargetRef) -> Retention {
    let storage = target::target(&r.kind).and_then(|tp| tp.default_retention(&r.name));
    contract::backup::resolve_retention(None, storage).value
}

/// Back up each provider (optionally narrowed to one instance), committing each
/// slot and pruning per the target's resolved `retention`. Failures are
/// collected, never fatal — a broken provider must not stop the rest.
async fn run_backups(
    store: &BackupStore,
    providers: &[Arc<dyn BackupProvider>],
    instance_filter: Option<&str>,
    retention: &Retention,
    ctx: &ToolCtx,
) -> BackupRunOutput {
    let mut out = BackupRunOutput::default();
    for p in providers {
        let instances: Vec<String> = match instance_filter {
            Some(i) => vec![i.to_string()],
            None => match p.instances() {
                Ok(v) => v,
                // A failed enumeration is a hard error, not "back up nothing":
                // record it so the run reports failure instead of a green run
                // that silently skipped every real instance of this kind.
                Err(e) => {
                    tracing::error!(
                        "[backup] {}: enumerate instances failed, skipping kind: {e:#}",
                        p.kind()
                    );
                    out.errors.push(BackupError {
                        kind: p.kind().to_string(),
                        instance: String::new(),
                        target: None,
                        error: format!("enumerate instances: {e:#}"),
                    });
                    continue;
                }
            },
        };
        for instance in instances {
            run_one(store, p, &instance, retention, ctx, &mut out).await;
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
    retention: &Retention,
    ctx: &ToolCtx,
    out: &mut BackupRunOutput,
) {
    let kind = p.kind();
    let collection = p.layout(instance);
    let slot = match store.new_slot(&collection, kind, instance) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[backup] {kind}/{instance}: cannot allocate slot: {e:#}");
            out.errors.push(BackupError {
                kind: kind.to_string(),
                instance: instance.to_string(),
                target: None,
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
                    target: None,
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
                target: None,
                error: format!("{e:#}"),
            });
        }
    }
    // Prune to the retention this target resolved to (its declared
    // `default_retention`, else the built-in default), NOT an unconditional
    // built-in default — otherwise a target asking to keep more than 25 / 1 GiB
    // silently loses backups it meant to keep.
    if let Err(e) = store.prune(kind, instance, retention) {
        tracing::warn!("[backup] {kind}/{instance}: prune failed: {e:#}");
    }
}

/// Restore one (kind, instance) with the surface-safe selection gate, searching
/// across every configured target.
async fn restore_one(
    kind: &str,
    instance: &str,
    id: Option<&str>,
    approve_all: bool,
    ctx: &ToolCtx,
) -> anyhow::Result<BackupRestoreOutput> {
    let p = provider::provider(kind)
        .ok_or_else(|| anyhow::anyhow!("no backup provider for kind `{kind}`"))?;
    let stores = open_configured_targets(ctx, true).await;

    let selector = match (id, approve_all) {
        (Some(i), _) => BackupSelector::parse(i),
        (None, true) => BackupSelector::Latest,
        (None, false) => {
            // Refuse: list the choices (across targets) instead of restoring blind.
            let mut available = Vec::new();
            for (_r, store) in &stores {
                if let Ok(mut recs) = store.list(Some(kind), Some(instance)) {
                    available.append(&mut recs);
                }
            }
            available.sort_by(|a, b| b.id.cmp(&a.id));
            return Ok(BackupRestoreOutput::AwaitingSelection {
                message: format!(
                    "restore of {kind}/{instance} needs a selection: pass --id <id> \
                     (from the list) or --approve-all to restore the latest"
                ),
                available,
            });
        }
    };

    // Pick the store+record that satisfies the selector. For `Latest`, that is
    // the newest record across all targets; for an explicit id, the first target
    // that holds it.
    let mut best: Option<BackupRecord> = None;
    for (_r, store) in &stores {
        if let Ok(rec) = store.resolve(kind, instance, &selector) {
            let take = match &best {
                Some(b) => rec.id > b.id,
                None => true,
            };
            if take {
                best = Some(rec);
            }
            if matches!(selector, BackupSelector::Id(_)) {
                break; // an explicit id is unique; first hit wins
            }
        }
    }
    let record = best.ok_or_else(|| anyhow::anyhow!("no matching backup for {kind}/{instance}"))?;

    let payload = PathBuf::from(&record.path);
    p.restore(&payload, instance, ctx)
        .await
        .map_err(|e| anyhow::anyhow!("restore {kind}/{instance} from {}: {e:#}", record.id))?;
    Ok(BackupRestoreOutput::Restored { record })
}

// ── target resolution ─────────────────────────────────────────────────

/// The targets `backup.run`/`list`/`restore` operate on: the `backup`/`targets`
/// config row, or the built-in `local` fallback when unset/empty.
fn configured_target_refs() -> Vec<BackupTargetRef> {
    #[derive(serde::Deserialize, Default)]
    struct TargetsRow {
        #[serde(default)]
        targets: Vec<BackupTargetRef>,
    }
    let read =
        db::pool::with_pooled_or_open(|conn| db::config_store::get(conn, "backup", "targets"));
    let refs = match read {
        Ok(Some(row)) => serde_json::from_str::<TargetsRow>(&row.json)
            .map(|r| r.targets)
            .unwrap_or_else(|e| {
                tracing::warn!("[backup] bad backup/targets config, using local: {e}");
                Vec::new()
            }),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!("[backup] cannot read backup/targets config, using local: {e}");
            Vec::new()
        }
    };
    if refs.is_empty() {
        vec![BackupTargetRef::local()]
    } else {
        refs
    }
}

/// Open every configured target to its store, skipping (log-and-continue) any
/// whose kind is not registered or fails to open. When `refresh` is true, each
/// target's remote backing is pulled first (for list/restore reads).
async fn open_configured_targets(
    ctx: &ToolCtx,
    refresh: bool,
) -> Vec<(BackupTargetRef, BackupStore)> {
    let mut out = Vec::new();
    for r in configured_target_refs() {
        let Some(tp) = target::target(&r.kind) else {
            tracing::warn!(
                "[backup] no target provider for kind `{}` (target {}/{}), skipping",
                r.kind,
                r.kind,
                r.name
            );
            continue;
        };
        if refresh && let Err(e) = tp.refresh(&r.name, ctx).await {
            tracing::warn!(
                "[backup] target {}/{} refresh failed: {e:#}",
                r.kind,
                r.name
            );
        }
        match tp.open(&r.name, ctx).await {
            Ok(store) => out.push((r, store)),
            Err(e) => tracing::warn!("[backup] target {}/{} open failed: {e:#}", r.kind, r.name),
        }
    }
    out
}

// ── fleet-wide collision machinery ─────────────────────────────────────

/// The `backup`/`destinations` config row: this owner's resolved destinations,
/// replicated so peers can detect fleet-wide collisions against them.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DestinationsRow {
    #[serde(default)]
    destinations: Vec<Destination>,
}

/// Self-report this host's destinations, then detect + reconcile fleet-wide
/// collisions. Returns the current collision set.
async fn refresh_and_check_collisions(ctx: &ToolCtx) -> anyhow::Result<Vec<collision::Collision>> {
    let owner = crate::host_identity::cli_hostname_or_fallback();
    let local = resolve_local_destinations(ctx).await;
    if let Err(e) = persist_destinations(&owner, &local) {
        tracing::warn!("[backup] persist destinations failed: {e:#}");
    }
    let all = gather_fleet_destinations()?;
    let collisions = collision::detect_collisions(&all);
    reconcile_collision_notifications(&collisions)?;
    Ok(collisions)
}

/// Resolve every (configured target × provider × instance) to a [`Destination`]:
/// the target's backing key plus the provider's layout sub-path.
async fn resolve_local_destinations(ctx: &ToolCtx) -> Vec<Destination> {
    let mut out = Vec::new();
    for r in configured_target_refs() {
        let Some(tp) = target::target(&r.kind) else {
            continue;
        };
        let backing_key = match tp.backing_key(&r.name, ctx).await {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("[backup] backing_key for {}/{}: {e:#}", r.kind, r.name);
                continue;
            }
        };
        let label = format!("{}/{}", r.kind, r.name);
        for p in provider::providers() {
            let instances = match p.instances() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "[backup] destinations: enumerate {}: {e:#}; skipping",
                        p.kind()
                    );
                    continue;
                }
            };
            for instance in instances {
                out.push(Destination {
                    kind: p.kind().to_string(),
                    subpath: p.layout(&instance).join("/"),
                    instance,
                    backing_key: backing_key.clone(),
                    target: label.clone(),
                });
            }
        }
    }
    out
}

/// Write this owner's destinations to the replicated `backup`/`destinations` row.
fn persist_destinations(owner: &str, dests: &[Destination]) -> anyhow::Result<()> {
    let row = DestinationsRow {
        destinations: dests.to_vec(),
    };
    let json = serde_json::to_string(&row)?;
    db::pool::with_pooled_or_open(|conn| {
        db::config_store::set(conn, owner, owner, "backup", "destinations", &json, owner)?;
        Ok(())
    })
}

/// Union of every owner's reported destinations (fleet-wide).
fn gather_fleet_destinations() -> anyhow::Result<Vec<OwnedDestination>> {
    let rows =
        db::pool::with_pooled_or_open(|conn| db::config_store::list(conn, Some("backup"), None))?;
    let mut out = Vec::new();
    for row in rows {
        if row.name != "destinations" {
            continue;
        }
        match serde_json::from_str::<DestinationsRow>(&row.json) {
            Ok(parsed) => {
                for dest in parsed.destinations {
                    out.push(OwnedDestination {
                        owner: row.host_owner.clone(),
                        dest,
                    });
                }
            }
            Err(e) => tracing::warn!("[backup] bad destinations row for {}: {e}", row.host_owner),
        }
    }
    Ok(out)
}

/// Raise a dismissable notification for each current collision and clear any
/// backup-collision notification whose condition no longer holds.
fn reconcile_collision_notifications(collisions: &[collision::Collision]) -> anyhow::Result<()> {
    use db::notifications_store as notify;
    let now = utils::time::now_millis_since_epoch();
    let current: std::collections::HashSet<String> = collisions.iter().map(|c| c.key()).collect();
    db::pool::with_pooled_or_open(|conn| {
        for c in collisions {
            notify::raise(
                conn,
                notify::RaiseInput {
                    key: c.key(),
                    source: "backup-collision".to_string(),
                    source_ref: Some(c.backing_key.clone()),
                    severity: notify::Severity::Warn,
                    actionable: true,
                    fix: None,
                    title: "Backup destination collision".to_string(),
                    body: Some(c.describe()),
                    user_id: None,
                },
                now,
            )?;
        }
        // Clear stale collisions we previously raised.
        let active = notify::list(
            conn,
            &notify::ListFilter {
                state: Some(notify::State::Active),
                audience: None,
            },
        )?;
        for n in active {
            if n.source == "backup-collision" && !current.contains(&n.key) {
                notify::dismiss(conn, &n.key, now)?;
            }
        }
        Ok(())
    })
}

/// Register the built-in (core-owned) backup KINDS and the built-in `local`
/// TARGET. Called once at daemon startup, alongside service-backend registration.
/// Core owns only `local`; plugins register additional target kinds.
pub fn register_builtin_providers() {
    provider::register_provider(Arc::new(HostBackupProvider::new()));
    provider::register_provider(Arc::new(ServiceKindProvider::new()));
    target::register_target(Arc::new(LocalTarget::new()));
    // Inject the out-of-process KIND/TARGET domain constructors into the plugin
    // loader so a subprocess plugin can contribute additional kinds/targets.
    // Must happen before plugins load — this runs at daemon startup.
    super::proxy::install();
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
        fn instances(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec!["default".into()])
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

    /// A provider whose instance enumeration always fails — models an OOP KIND
    /// whose `instances` op errored (socket hiccup, bad JSON).
    struct FailEnumProvider;
    impl BackupProvider for FailEnumProvider {
        fn kind(&self) -> &str {
            "failenum"
        }
        fn instances(&self) -> anyhow::Result<Vec<String>> {
            Err(anyhow::anyhow!("enumeration boom"))
        }
        fn backup<'a>(
            &'a self,
            _payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<super::super::provider::BackupOutcome>>
        {
            Box::pin(async move { panic!("backup must not run when enumeration failed") })
        }
        fn restore<'a>(
            &'a self,
            _payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    /// A target advertising a custom retention (keep only the last 2).
    struct RetentionTarget;
    impl super::super::target::BackupTargetProvider for RetentionTarget {
        fn kind(&self) -> &str {
            "rtn-test"
        }
        fn open<'a>(
            &'a self,
            _name: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<BackupStore>> {
            Box::pin(async { anyhow::bail!("open not used in this test") })
        }
        fn default_retention(&self, _name: &str) -> Option<Retention> {
            Some(Retention {
                keep_last: Some(2),
                ..Default::default()
            })
        }
    }

    #[test]
    fn target_declared_retention_reaches_resolution() {
        target::register_target(Arc::new(RetentionTarget));
        // A target that declares retention gets it applied (not the built-in 25).
        let resolved = resolve_target_retention(&BackupTargetRef::new("rtn-test", "default"));
        assert_eq!(resolved.keep_last, Some(2));
        // A target with no declared retention falls back to the built-in default.
        let fallback = resolve_target_retention(&BackupTargetRef::new("no-such-kind", "default"));
        assert_eq!(fallback, Retention::default());
        target::deregister_target("rtn-test");
    }

    #[tokio::test]
    async fn failed_enumeration_is_recorded_not_silently_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> = vec![Arc::new(FailEnumProvider)];
        let out = run_backups(&store, &providers, None, &Retention::default(), &ctx()).await;
        // Nothing captured, but the failure is LOUD in the run output — not a
        // green run that silently skipped every instance.
        assert!(out.produced.is_empty());
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].kind, "failenum");
        assert!(out.errors[0].error.contains("enumerate instances"));
    }

    #[tokio::test]
    async fn run_then_list_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> = vec![Arc::new(StubProvider {
            kind: "stub".into(),
        })];
        let ctx = ctx();

        let out = run_backups(&store, &providers, None, &Retention::default(), &ctx).await;
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
    fn run_without_kind_or_all_refuses() {
        // All-kinds is opt-in: neither --kind nor --all must refuse, not fan out.
        let err = match resolve_run_providers(None, false) {
            Ok(_) => panic!("expected a refusal when neither --kind nor --all is set"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("--kind"));
        assert!(err.contains("--all"));
        // --all opts in explicitly (empty registry here → empty set, no error).
        assert!(resolve_run_providers(None, true).is_ok());
    }

    #[test]
    fn builtin_providers_register_host_service_and_local_target() {
        register_builtin_providers();
        assert!(provider::provider("host").is_some());
        assert!(provider::provider("service").is_some());
        // Core owns exactly the built-in `local` target.
        let local = target::target("local").expect("local target registered");
        assert_eq!(local.title(), "Local filesystem");
        assert!(local.fits(&Placement::bare()), "local fits anywhere");
    }

    #[test]
    fn placement_is_bare_without_config() {
        // With no `backup/placement` config row (the unit-test env), placement
        // carries no labels — core detects nothing platform-specific itself.
        assert!(target::placement().labels.is_empty());
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

    // ── view enum: default + wire spelling ─────────────────────────────

    #[test]
    fn backup_detail_view_default_is_providers() {
        assert_eq!(BackupDetailView::default(), BackupDetailView::Providers);
        // camelCase wire spelling for both variants.
        assert_eq!(
            serde_json::to_string(&BackupDetailView::Providers).unwrap(),
            "\"providers\""
        );
        assert_eq!(
            serde_json::to_string(&BackupDetailView::Targets).unwrap(),
            "\"targets\""
        );
        // Args default the view to providers.
        let args = BackupDetailArgs::default();
        assert_eq!(args.view, BackupDetailView::Providers);
    }

    // ── serde output shapes (camelCase, skip_serializing_if, untagged) ──

    #[test]
    fn provider_info_serializes_camel_case() {
        let info = ProviderInfo {
            kind: "host".into(),
            title: "Host".into(),
            instances: vec!["default".into(), "thor".into()],
        };
        let s = serde_json::to_string(&info).unwrap();
        assert_eq!(
            s,
            r#"{"kind":"host","title":"Host","instances":["default","thor"]}"#
        );
    }

    #[test]
    fn backup_detail_output_providers_is_untagged() {
        // The untagged enum must serialize as the bare ProvidersOutput shape —
        // no enum discriminant wrapper — so callers key on `providers`.
        let out = BackupDetailOutput::Providers(ProvidersOutput {
            providers: vec![ProviderInfo {
                kind: "host".into(),
                title: "Host".into(),
                instances: vec![],
            }],
        });
        let s = serde_json::to_string(&out).unwrap();
        assert_eq!(
            s,
            r#"{"providers":[{"kind":"host","title":"Host","instances":[]}]}"#
        );
    }

    #[test]
    fn target_info_and_output_serialize_camel_case() {
        let info = TargetInfo {
            kind: "local".into(),
            title: "Local filesystem".into(),
            fits_here: true,
            builtin: true,
            locations: vec![],
        };
        let s = serde_json::to_string(&info).unwrap();
        assert!(s.contains(r#""fitsHere":true"#));
        assert!(s.contains(r#""builtin":true"#));
        assert!(s.contains(r#""locations":[]"#));

        let out = TargetsOutput {
            registered: vec![info],
            configured: vec![BackupTargetRef::local()],
            placement: Placement::bare(),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""registered":["#));
        assert!(s.contains(r#""configured":[{"kind":"local","name":"default"}]"#));
        assert!(s.contains(r#""placement":{"labels":[]}"#));
    }

    #[test]
    fn backup_error_omits_target_when_none() {
        let e = BackupError {
            kind: "host".into(),
            instance: "default".into(),
            target: None,
            error: "boom".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("target"), "None target is skipped: {s}");

        let e = BackupError {
            target: Some("local/default".into()),
            ..e
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""target":"local/default""#));
    }

    #[test]
    fn backup_run_output_default_is_empty() {
        let out = BackupRunOutput::default();
        assert!(out.produced.is_empty());
        assert!(out.targets.is_empty());
        assert!(out.errors.is_empty());
        let s = serde_json::to_string(&out).unwrap();
        assert_eq!(s, r#"{"produced":[],"targets":[],"errors":[]}"#);
    }

    #[test]
    fn backup_list_output_skips_absent_cursor_and_total() {
        let out = BackupListOutput {
            backups: vec![],
            next_cursor: None,
            total: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert_eq!(s, r#"{"backups":[]}"#);

        let out = BackupListOutput {
            backups: vec![],
            next_cursor: Some("abc".into()),
            total: Some(3),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""nextCursor":"abc""#));
        assert!(s.contains(r#""total":3"#));
    }

    #[test]
    fn collision_info_and_check_output_serialize_camel_case() {
        let ci = CollisionInfo {
            backing_key: "nfs://nas/b".into(),
            party_a: "thor:host/thor".into(),
            party_b: "mimir:host/mimir".into(),
            path_a: "hosts/x".into(),
            path_b: "hosts/x".into(),
            nested: false,
        };
        let s = serde_json::to_string(&ci).unwrap();
        assert!(s.contains(r#""backingKey":"nfs://nas/b""#));
        assert!(s.contains(r#""partyA":"thor:host/thor""#));
        assert!(s.contains(r#""nested":false"#));

        let empty = BackupCheckOutput::default();
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"collisions":[]}"#
        );
    }

    // ── run-args / restore-args parsing surface ────────────────────────

    #[test]
    fn run_args_all_defaults_false() {
        let args = BackupRunArgs::default();
        assert!(args.kind.is_none());
        assert!(args.instance.is_none());
        assert!(!args.all);
    }

    #[test]
    fn restore_args_default_instance_is_default_const() {
        // A restore with no explicit instance resolves to DEFAULT_INSTANCE.
        let args = BackupRestoreArgs::default();
        let instance = args.instance.as_deref().unwrap_or(DEFAULT_INSTANCE);
        assert_eq!(instance, "default");
        assert!(!args.approve_all);
    }

    // ── provider resolution branches ───────────────────────────────────

    #[test]
    fn resolve_providers_named_success_returns_one() {
        let kind = "resolve-one-stub";
        provider::register_provider(Arc::new(StubProvider { kind: kind.into() }));
        let got = resolve_providers(Some(kind)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind(), kind);
        provider::deregister_provider(kind);
    }

    #[test]
    fn resolve_run_providers_named_kind_resolves_that_one() {
        let kind = "run-one-stub";
        provider::register_provider(Arc::new(StubProvider { kind: kind.into() }));
        // A named --kind resolves exactly that provider, --all irrelevant.
        let got = resolve_run_providers(Some(kind), false).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind(), kind);
        // An unknown --kind still errors even with --all set.
        assert!(resolve_run_providers(Some("no-such-run-kind"), true).is_err());
        provider::deregister_provider(kind);
    }

    // ── backup_providers listing tolerance ─────────────────────────────

    #[tokio::test]
    async fn backup_providers_maps_fields_and_tolerates_enum_failure() {
        let good = "listing-good-stub";
        provider::register_provider(Arc::new(StubProvider { kind: good.into() }));
        provider::register_provider(Arc::new(FailEnumProvider));

        let out = backup_providers().await;
        let g = out
            .providers
            .iter()
            .find(|p| p.kind == good)
            .expect("good provider listed");
        assert_eq!(g.instances, vec!["default".to_string()]);

        // A provider whose enumeration fails shows NO instances rather than
        // failing the whole surface (log-and-continue tolerance).
        let f = out
            .providers
            .iter()
            .find(|p| p.kind == "failenum")
            .expect("failing provider still listed");
        assert!(f.instances.is_empty());

        provider::deregister_provider(good);
        provider::deregister_provider("failenum");
    }

    #[tokio::test]
    async fn backup_detail_providers_view_returns_providers_variant() {
        let res = backup_detail(
            BackupDetailArgs {
                view: BackupDetailView::Providers,
            },
            &ctx(),
        )
        .await
        .unwrap();
        assert!(matches!(res, BackupDetailOutput::Providers(_)));
    }

    // ── configured targets fallback ────────────────────────────────────

    #[test]
    fn configured_target_refs_defaults_to_local_without_config() {
        // With no `backup/targets` config row, the fleet falls back to the
        // always-available built-in local target.
        let refs = configured_target_refs();
        assert_eq!(refs.len(), 1);
        assert!(refs[0].is_local());
        assert_eq!(refs[0].name, "default");
    }

    #[test]
    fn resolve_target_retention_ignores_backup_tier_on_manual_run() {
        // A manual run has no per-backup override, so an unregistered target
        // kind resolves to the built-in default (keep 25, 1 GiB cap).
        let r = resolve_target_retention(&BackupTargetRef::local());
        assert_eq!(r, Retention::default());
        assert_eq!(r.keep_last, Some(25));
    }

    // ── additional fakes ───────────────────────────────────────────────

    /// A provider whose `backup` op always fails — models a capture that errored
    /// mid-run. The tool layer must abort the slot and record the failure.
    struct FailBackupProvider {
        kind: String,
    }
    impl BackupProvider for FailBackupProvider {
        fn kind(&self) -> &str {
            &self.kind
        }
        fn instances(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec!["default".into()])
        }
        fn backup<'a>(
            &'a self,
            _payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<super::super::provider::BackupOutcome>>
        {
            Box::pin(async move { anyhow::bail!("capture exploded") })
        }
        fn restore<'a>(
            &'a self,
            _payload_dir: &'a Path,
            _instance: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    // ── run_backups: instance filter, backup-failure, and mixed fan-out ─

    #[tokio::test]
    async fn run_backups_honors_instance_filter() {
        // With an explicit instance filter, that instance is backed up verbatim
        // — the provider's own enumeration is bypassed.
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> =
            vec![Arc::new(StubProvider { kind: "flt".into() })];
        let out = run_backups(
            &store,
            &providers,
            Some("custom"),
            &Retention::default(),
            &ctx(),
        )
        .await;
        assert_eq!(out.produced.len(), 1);
        assert!(out.errors.is_empty());
        assert_eq!(out.produced[0].instance, "custom");
    }

    #[tokio::test]
    async fn run_backups_records_backup_failure_and_aborts_slot() {
        // A provider whose capture fails is recorded as an error, produces no
        // record, and leaves no committed backup behind (the slot is aborted).
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> = vec![Arc::new(FailBackupProvider {
            kind: "failbk".into(),
        })];
        let out = run_backups(&store, &providers, None, &Retention::default(), &ctx()).await;
        assert!(out.produced.is_empty());
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].kind, "failbk");
        assert_eq!(out.errors[0].instance, "default");
        assert!(out.errors[0].error.contains("capture exploded"));
        // Nothing was committed, so listing sees no completed backup.
        assert!(
            store
                .list(Some("failbk"), Some("default"))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn run_backups_mixed_providers_isolate_failures() {
        // A broken provider must not stop the others: one good record, one error.
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> = vec![
            Arc::new(StubProvider {
                kind: "mix-ok".into(),
            }),
            Arc::new(FailEnumProvider),
        ];
        let out = run_backups(&store, &providers, None, &Retention::default(), &ctx()).await;
        assert_eq!(out.produced.len(), 1);
        assert_eq!(out.produced[0].kind, "mix-ok");
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].kind, "failenum");
    }

    #[tokio::test]
    async fn run_backups_prunes_to_retention_keep_last() {
        // Two runs of the same instance with keep_last=1 leaves exactly one
        // backup — the newest — after pruning.
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path().join("b"));
        let providers: Vec<Arc<dyn BackupProvider>> = vec![Arc::new(StubProvider {
            kind: "prune-kind".into(),
        })];
        let keep_one = Retention::keep_last(1);
        run_backups(&store, &providers, None, &keep_one, &ctx()).await;
        // A second slot in the same second is disambiguated by the store.
        run_backups(&store, &providers, None, &keep_one, &ctx()).await;
        let listed = store.list(Some("prune-kind"), Some("default")).unwrap();
        assert_eq!(listed.len(), 1, "keep_last=1 prunes older backups");
    }

    // ── resolve_providers / resolve_run_providers: all-kinds branches ───

    #[test]
    fn resolve_providers_none_returns_all_registered() {
        let kind = "all-list-stub";
        provider::register_provider(Arc::new(StubProvider { kind: kind.into() }));
        let got = resolve_providers(None).unwrap();
        assert!(got.iter().any(|p| p.kind() == kind));
        provider::deregister_provider(kind);
    }

    #[test]
    fn resolve_run_providers_all_includes_registered_kind() {
        let kind = "all-run-stub";
        provider::register_provider(Arc::new(StubProvider { kind: kind.into() }));
        let got = resolve_run_providers(None, true).unwrap();
        assert!(got.iter().any(|p| p.kind() == kind));
        provider::deregister_provider(kind);
    }

    #[test]
    fn resolve_run_providers_refusal_lists_registered_kinds() {
        // The refusal names the registered kinds so a caller can choose one.
        let kind = "refusal-list-stub";
        provider::register_provider(Arc::new(StubProvider { kind: kind.into() }));
        let err = match resolve_run_providers(None, false) {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(kind), "refusal enumerates kinds: {err}");
        provider::deregister_provider(kind);
    }

    // ── restore_one: error branches ────────────────────────────────────

    #[tokio::test]
    async fn restore_one_unknown_kind_errors() {
        // No provider for the kind → hard error before touching any store.
        let err = restore_one(
            "no-such-restore-kind",
            "default",
            Some("latest"),
            false,
            &ctx(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("no backup provider"));
    }

    #[tokio::test]
    async fn restore_one_missing_id_errors_with_no_match() {
        // A concrete id that no target holds yields a "no matching backup" error,
        // not a blind restore.
        let kind = "restore-missing-stub";
        provider::register_provider(Arc::new(StubProvider { kind: kind.into() }));
        let err = restore_one(kind, "default", Some("20200101-000000"), false, &ctx())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no matching backup"), "got: {err}");
        provider::deregister_provider(kind);
    }

    // ── args parsing surface ───────────────────────────────────────────

    #[test]
    fn list_args_default_all_none() {
        let args = BackupListArgs::default();
        assert!(args.kind.is_none());
        assert!(args.instance.is_none());
        assert!(args.limit.is_none());
        assert!(args.cursor.is_none());
    }

    #[test]
    fn restore_args_explicit_id_and_approve_all_parse() {
        let args = BackupRestoreArgs {
            kind: "host".into(),
            instance: Some("thor".into()),
            id: Some("20260101-000000".into()),
            approve_all: true,
        };
        assert_eq!(args.instance.as_deref(), Some("thor"));
        assert_eq!(args.id.as_deref(), Some("20260101-000000"));
        assert!(args.approve_all);
    }

    // ── serde output shapes not already covered ────────────────────────

    #[test]
    fn awaiting_selection_serializes_message_and_available() {
        let out = BackupRestoreOutput::AwaitingSelection {
            message: "pick one".into(),
            available: vec![BackupRecord {
                id: "20260101-000000".into(),
                kind: "host".into(),
                instance: "default".into(),
                created_ms: 1,
                path: "/p".into(),
                size_bytes: 0,
                file_count: 0,
                checksum: None,
                note: None,
            }],
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""status":"awaitingSelection""#));
        assert!(s.contains(r#""message":"pick one""#));
        assert!(s.contains(r#""available":[{"#));
    }

    #[test]
    fn collision_info_nested_true_serializes() {
        let ci = CollisionInfo {
            backing_key: "nfs://nas/b".into(),
            party_a: "a".into(),
            party_b: "b".into(),
            path_a: "hosts".into(),
            path_b: "hosts/x".into(),
            nested: true,
        };
        let s = serde_json::to_string(&ci).unwrap();
        assert!(s.contains(r#""nested":true"#));
        assert!(s.contains(r#""pathB":"hosts/x""#));
    }

    #[test]
    fn providers_output_empty_serializes_as_empty_list() {
        let out = ProvidersOutput { providers: vec![] };
        assert_eq!(serde_json::to_string(&out).unwrap(), r#"{"providers":[]}"#);
    }

    #[test]
    fn provider_info_empty_instances_serializes() {
        let info = ProviderInfo {
            kind: "k".into(),
            title: "K".into(),
            instances: vec![],
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"kind":"k","title":"K","instances":[]}"#
        );
    }

    // ── detail: targets view ───────────────────────────────────────────

    // ── DB-backed helpers: isolated thread-local sqlite ────────────────────
    //
    // `with_thread_db_path` scopes a private on-disk db to this test thread so
    // these never race the rest of the suite. `open_default` runs the core
    // schema; `apply_fragments` materialises the endpoint tables.

    fn with_db<F: FnOnce()>(name: &str, f: F) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            db::schema_fragments::apply_fragments(&conn).expect("apply fragments");
            drop(conn);
            f();
        });
    }

    fn dest(kind: &str, instance: &str, backing: &str, subpath: &str) -> Destination {
        Destination {
            kind: kind.into(),
            instance: instance.into(),
            backing_key: backing.into(),
            subpath: subpath.into(),
            target: format!("{kind}/default"),
        }
    }

    #[test]
    fn configured_target_refs_reads_a_set_config_row() {
        with_db("cfg_targets.db", || {
            // A well-formed backup/targets row overrides the local fallback.
            let json = r#"{"targets":[{"kind":"nfs","name":"nas"}]}"#;
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", json, "h")
            })
            .expect("set config row");

            let refs = configured_target_refs();
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].kind, "nfs");
            assert_eq!(refs[0].name, "nas");
            assert!(!refs[0].is_local());
        });
    }

    #[test]
    fn configured_target_refs_falls_back_to_local_on_bad_shape() {
        with_db("cfg_targets_bad.db", || {
            // Valid JSON but the wrong shape for TargetsRow (targets is a string):
            // the parse fails, is logged, and the local fallback is used.
            let json = r#"{"targets":"not-an-array"}"#;
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", json, "h")
            })
            .expect("set config row");

            let refs = configured_target_refs();
            assert_eq!(refs.len(), 1);
            assert!(refs[0].is_local());
        });
    }

    #[test]
    fn persist_and_gather_destinations_round_trip() {
        with_db("dests.db", || {
            let owner = "thor";
            let dests = vec![
                dest("host", "thor", "nfs://nas/b", "hosts/thor"),
                dest("service", "abs", "nfs://nas/b", "services/abs"),
            ];
            persist_destinations(owner, &dests).expect("persist destinations");

            let gathered = gather_fleet_destinations().expect("gather destinations");
            assert_eq!(gathered.len(), 2);
            assert!(gathered.iter().all(|g| g.owner == owner));
            assert!(
                gathered
                    .iter()
                    .any(|g| g.dest.kind == "host" && g.dest.subpath == "hosts/thor")
            );
            assert!(
                gathered
                    .iter()
                    .any(|g| g.dest.kind == "service" && g.dest.subpath == "services/abs")
            );
        });
    }

    #[test]
    fn reconcile_collision_notifications_raises_then_clears() {
        use db::notifications_store as notify;
        with_db("collisions.db", || {
            let collision = collision::Collision {
                backing_key: "nfs://nas/b".into(),
                party_a: "thor:host/thor".into(),
                party_b: "mimir:host/mimir".into(),
                path_a: "hosts/x".into(),
                path_b: "hosts/x".into(),
                nested: false,
            };
            let key = collision.key();

            // First pass: the collision is active → a notification is raised.
            reconcile_collision_notifications(std::slice::from_ref(&collision))
                .expect("reconcile raise");
            let active = db::pool::with_pooled_or_open(|conn| {
                notify::list(
                    conn,
                    &notify::ListFilter {
                        state: Some(notify::State::Active),
                        audience: None,
                    },
                )
            })
            .expect("list active");
            let raised = active
                .iter()
                .find(|n| n.key == key)
                .expect("collision notification raised");
            assert_eq!(raised.source, "backup-collision");

            // Second pass: no collisions → the stale notification is dismissed.
            reconcile_collision_notifications(&[]).expect("reconcile clear");
            let still_active = db::pool::with_pooled_or_open(|conn| {
                notify::list(
                    conn,
                    &notify::ListFilter {
                        state: Some(notify::State::Active),
                        audience: None,
                    },
                )
            })
            .expect("list active after clear");
            assert!(
                !still_active.iter().any(|n| n.key == key),
                "the resolved collision must be dismissed"
            );
        });
    }

    // ── target provider fake rooted at a real dir ──────────────────────
    //
    // Opens to a store beneath a fixed root so the same backups are visible
    // across repeated `open` calls (as list/restore need), and can advertise
    // locations or fail enumeration to exercise the tolerance paths.
    struct RootedTarget {
        kind: String,
        root: PathBuf,
        locations: Vec<TargetLocation>,
        fail_available: bool,
    }
    impl super::super::target::BackupTargetProvider for RootedTarget {
        fn kind(&self) -> &str {
            &self.kind
        }
        fn title(&self) -> &str {
            "Rooted test target"
        }
        fn open<'a>(
            &'a self,
            _name: &'a str,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<BackupStore>> {
            let root = self.root.clone();
            Box::pin(async move { Ok(BackupStore::new(root)) })
        }
        fn available<'a>(
            &'a self,
            _ctx: &'a ToolCtx,
        ) -> contract::BoxFuture<'a, anyhow::Result<Vec<TargetLocation>>> {
            let fail = self.fail_available;
            let locs = self.locations.clone();
            Box::pin(async move {
                if fail {
                    anyhow::bail!("available() boom");
                }
                Ok(locs)
            })
        }
    }

    fn location(id: &str) -> TargetLocation {
        TargetLocation {
            id: id.into(),
            label: format!("loc {id}"),
            base_path: Some(format!("/mnt/{id}")),
            backing_key: format!("test://{id}"),
        }
    }

    #[tokio::test]
    async fn backup_targets_lists_registered_target_with_locations() {
        let kind = "rooted-locs";
        target::register_target(Arc::new(RootedTarget {
            kind: kind.into(),
            root: PathBuf::from("/tmp/unused"),
            locations: vec![location("nas")],
            fail_available: false,
        }));
        let out = backup_targets(&ctx()).await.unwrap();
        let ti = out
            .registered
            .iter()
            .find(|t| t.kind == kind)
            .expect("registered target listed");
        assert_eq!(ti.title, "Rooted test target");
        assert!(!ti.builtin, "a non-local kind is not builtin");
        assert!(ti.fits_here, "default fits() is true everywhere");
        assert_eq!(ti.locations.len(), 1);
        assert_eq!(ti.locations[0].id, "nas");
        target::deregister_target(kind);
    }

    #[tokio::test]
    async fn backup_targets_tolerates_available_failure() {
        let kind = "rooted-avail-fail";
        target::register_target(Arc::new(RootedTarget {
            kind: kind.into(),
            root: PathBuf::from("/tmp/unused"),
            locations: vec![location("x")],
            fail_available: true,
        }));
        let out = backup_targets(&ctx()).await.unwrap();
        let ti = out
            .registered
            .iter()
            .find(|t| t.kind == kind)
            .expect("target still listed despite available() error");
        assert!(
            ti.locations.is_empty(),
            "a failing available() yields no locations, not a failed surface"
        );
        target::deregister_target(kind);
    }

    /// A current-thread runtime so async helpers run on THIS thread — the one
    /// `with_db` scoped the thread-local db to. `#[tokio::test]` can't be used
    /// here: a nested `block_on` inside the sync `with_db` closure would panic.
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    #[test]
    fn open_configured_targets_opens_known_and_skips_unknown() {
        let kind = "open-known";
        let tmp = tempfile::tempdir().unwrap();
        target::register_target(Arc::new(RootedTarget {
            kind: kind.into(),
            root: tmp.path().to_path_buf(),
            locations: vec![],
            fail_available: false,
        }));
        with_db("open_targets.db", || {
            // Config names one registered kind and one that has no provider.
            let json = format!(
                r#"{{"targets":[{{"kind":"{kind}","name":"default"}},{{"kind":"missing-kind","name":"nas"}}]}}"#
            );
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", &json, "h")
            })
            .expect("set config row");
            let opened = rt().block_on(open_configured_targets(&ctx(), false));
            assert_eq!(opened.len(), 1, "unknown kind is skipped, known is opened");
            assert_eq!(opened[0].0.kind, kind);
        });
        target::deregister_target(kind);
    }

    #[test]
    fn resolve_local_destinations_maps_targets_by_provider_instance() {
        let tkind = "rooted-dests";
        let pkind = "prov-dests";
        target::register_target(Arc::new(RootedTarget {
            kind: tkind.into(),
            root: PathBuf::from("/tmp/unused"),
            locations: vec![],
            fail_available: false,
        }));
        provider::register_provider(Arc::new(StubProvider { kind: pkind.into() }));
        with_db("resolve_dests.db", || {
            let json = format!(r#"{{"targets":[{{"kind":"{tkind}","name":"default"}}]}}"#);
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", &json, "h")
            })
            .expect("set config row");
            let dests = rt().block_on(resolve_local_destinations(&ctx()));
            let d = dests
                .iter()
                .find(|d| d.kind == pkind)
                .expect("provider instance mapped to a destination");
            // default backing_key is `<kind>://<name>`; subpath is the provider layout.
            assert_eq!(d.backing_key, format!("{tkind}://default"));
            assert_eq!(d.instance, "default");
            assert_eq!(d.subpath, format!("{pkind}/default"));
            assert_eq!(d.target, format!("{tkind}/default"));
        });
        target::deregister_target(tkind);
        provider::deregister_provider(pkind);
    }

    #[test]
    fn restore_one_restores_latest_from_configured_target() {
        let tkind = "rooted-restore";
        let pkind = "prov-restore";
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        target::register_target(Arc::new(RootedTarget {
            kind: tkind.into(),
            root: root.clone(),
            locations: vec![],
            fail_available: false,
        }));
        provider::register_provider(Arc::new(StubProvider { kind: pkind.into() }));
        with_db("restore_success.db", || {
            let json = format!(r#"{{"targets":[{{"kind":"{tkind}","name":"default"}}]}}"#);
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", &json, "h")
            })
            .expect("set config row");
            // Seed one real backup into the same store the target opens.
            let store = BackupStore::new(root.clone());
            let providers: Vec<Arc<dyn BackupProvider>> =
                vec![Arc::new(StubProvider { kind: pkind.into() })];
            let out = rt().block_on(run_backups(
                &store,
                &providers,
                None,
                &Retention::default(),
                &ctx(),
            ));
            assert_eq!(out.produced.len(), 1);
            let seeded = out.produced[0].id.clone();

            // approve_all restores the latest across configured targets.
            let res = rt()
                .block_on(restore_one(pkind, "default", None, true, &ctx()))
                .expect("restore succeeds");
            match res {
                BackupRestoreOutput::Restored { record } => {
                    assert_eq!(record.kind, pkind);
                    assert_eq!(record.id, seeded);
                }
                BackupRestoreOutput::AwaitingSelection { .. } => {
                    panic!("approve_all must restore, not await")
                }
            }
        });
        target::deregister_target(tkind);
        provider::deregister_provider(pkind);
    }

    #[tokio::test]
    async fn backup_detail_targets_view_returns_targets_variant() {
        let res = backup_detail(
            BackupDetailArgs {
                view: BackupDetailView::Targets,
            },
            &ctx(),
        )
        .await
        .unwrap();
        assert!(matches!(res, BackupDetailOutput::Targets(_)));
    }

    // ── backup.run tool body: fan out to a configured target end-to-end ────

    #[test]
    fn backup_run_writes_to_configured_target_and_labels_it() {
        // Drive the `backup.run` tool body (not just run_backups): it resolves the
        // named provider, opens the one configured target, captures a backup,
        // labels the run with the target, and runs the best-effort collision
        // check — all without aborting. A named --kind keeps it deterministic
        // regardless of what else is in the process-global provider registry.
        let tkind = "run-tool-tgt";
        let pkind = "run-tool-prov";
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        target::register_target(Arc::new(RootedTarget {
            kind: tkind.into(),
            root: root.clone(),
            locations: vec![],
            fail_available: false,
        }));
        provider::register_provider(Arc::new(StubProvider { kind: pkind.into() }));
        with_db("backup_run_tool.db", || {
            let json = format!(r#"{{"targets":[{{"kind":"{tkind}","name":"default"}}]}}"#);
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", &json, "h")
            })
            .expect("set config row");

            let out = rt()
                .block_on(backup_run(
                    BackupRunArgs {
                        kind: Some(pkind.into()),
                        instance: None,
                        all: false,
                    },
                    &ctx(),
                ))
                .expect("backup_run succeeds");

            assert_eq!(out.produced.len(), 1, "one instance captured");
            assert_eq!(out.produced[0].kind, pkind);
            assert_eq!(out.produced[0].instance, "default");
            assert_eq!(
                out.targets,
                vec![format!("{tkind}/default")],
                "the run is labeled with the configured target"
            );
            assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
            // The backup really landed in the target's on-disk store.
            let store = BackupStore::new(root.clone());
            assert_eq!(
                store.list(Some(pkind), Some("default")).unwrap().len(),
                1,
                "the committed backup is listable in the target store"
            );
        });
        target::deregister_target(tkind);
        provider::deregister_provider(pkind);
    }

    // ── backup.check tool body: a real fleet-wide collision is mapped out ──

    #[test]
    fn backup_check_maps_a_detected_collision() {
        // A DIFFERENT owner already writes the exact backing+sub-path this host
        // resolves to. `backup.check` must re-publish our destinations, union the
        // fleet's, detect the same-folder clash, and map it into the output.
        let tkind = "chk-tool-tgt";
        let pkind = "chk-tool-prov";
        target::register_target(Arc::new(RootedTarget {
            kind: tkind.into(),
            root: PathBuf::from("/tmp/unused-chk"),
            locations: vec![],
            fail_available: false,
        }));
        provider::register_provider(Arc::new(StubProvider { kind: pkind.into() }));
        with_db("backup_check_collision.db", || {
            let json = format!(r#"{{"targets":[{{"kind":"{tkind}","name":"default"}}]}}"#);
            db::pool::with_pooled_or_open(|conn| {
                db::config_store::set(conn, "h", "h", "backup", "targets", &json, "h")
            })
            .expect("set config row");

            // What THIS host resolves for the stub provider on the configured target.
            let local = rt().block_on(resolve_local_destinations(&ctx()));
            let mine = local
                .iter()
                .find(|d| d.kind == pkind)
                .expect("stub provider resolves a destination")
                .clone();

            // A foreign peer already claims the identical destination → collision.
            persist_destinations("foreign-peer-xyz", std::slice::from_ref(&mine))
                .expect("persist foreign destination");

            let out = rt()
                .block_on(backup_check(BackupCheckArgs {}, &ctx()))
                .expect("backup_check succeeds");

            let hit = out
                .collisions
                .iter()
                .find(|c| c.backing_key == mine.backing_key)
                .expect("the same-folder collision is reported");
            assert_eq!(hit.path_a, mine.subpath);
            assert_eq!(hit.path_b, mine.subpath);
            assert!(!hit.nested, "identical paths collide, not nest: {hit:?}");
        });
        target::deregister_target(tkind);
        provider::deregister_provider(pkind);
    }
}
