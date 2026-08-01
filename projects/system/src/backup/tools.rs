//! Generic backup/restore tool surface — a single `backup` domain, parameterized
//! by `--kind` (mirroring how `service.*` is generic-over-service):
//!
//! * `backup.providers` — every registered backup kind + its instances.
//! * `backup.targets`   — registered target kinds, placement fit, and the
//!   concrete locations each exposes for selection.
//! * `backup.list`      — dated backups (all, or narrowed by kind/instance).
//! * `backup.run`       — run one kind, or ALL when `--kind` is omitted (this is
//!   `orca backup`). Fans out log-and-skip, per [[fail-loud-logging-levels]].
//!   Writes each backup under a provider-declared `category/class/name` layout
//!   beneath every configured target root, then checks the fleet for collisions.
//! * `backup.restore`   — date-selected restore with surface-safe gating.
//! * `backup.check`     — fleet-wide same-folder collision detection; raises a
//!   dismissable notification per collision ([[dismissable-notifications-subsystem]]).
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TargetsArgs {}

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

/// Every registered backup target kind, which fit the current placement, and the
/// targets backups currently fan out to.
#[orca_tool(domain = "backup", verb = "targets")]
async fn backup_targets(_args: TargetsArgs, ctx: &ToolCtx) -> anyhow::Result<TargetsOutput> {
    let placement = target::detect_placement();
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
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BackupListOutput {
    /// Matching backups, newest first.
    pub backups: Vec<BackupRecord>,
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
    Ok(BackupListOutput { backups })
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
    /// Kind to back up (e.g. `host`). Omit to back up EVERY registered kind —
    /// this is `orca backup`.
    #[arg(long)]
    pub kind: Option<String>,
    /// Instance to back up. Omit for every instance the kind advertises.
    #[arg(long)]
    pub instance: Option<String>,
}

/// Run backups. With `--kind` it backs up that kind; without, it fans out over
/// every registered kind (log-and-skip on failure). Backups are written to EVERY
/// configured target (the `backup`/`targets` config, or the built-in `local`
/// fallback); old backups beyond the retention policy are pruned per instance,
/// per target.
#[orca_tool(domain = "backup", verb = "run", data_mutation = true, role = "admin")]
async fn backup_run(args: BackupRunArgs, ctx: &ToolCtx) -> anyhow::Result<BackupRunOutput> {
    let providers = resolve_providers(args.kind.as_deref())?;
    let mut out = BackupRunOutput::default();

    for (r, store) in open_configured_targets(ctx, false).await {
        let label = format!("{}/{}", r.kind, r.name);
        let mut sub = run_backups(&store, &providers, args.instance.as_deref(), ctx).await;
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
    if let Err(e) = store.prune(kind, instance, &Retention::default()) {
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
            for instance in p.instances() {
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
    fn detect_placement_is_bare_without_proxmox_markers() {
        // The test host has neither /etc/pve nor PBS env, so placement is bare.
        // (Guarded so it does not assert falsely on an actual Proxmox CI host.)
        if !std::path::Path::new("/etc/pve").is_dir()
            && std::env::var_os("PBS_REPOSITORY").is_none()
            && std::env::var_os("ORCA_PBS_STORAGE").is_none()
        {
            assert!(!target::detect_placement().proxmox);
        }
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
