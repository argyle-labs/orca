//! ConfigSource domain tools — the git-repo ⇆ config-store reconcile surface.
//!
//! Six-verb surface. Tonight only the READ-ONLY slice is implemented:
//!   - `configsource.status` — liveness + checkout + row counts.
//!   - `configsource.diff`   — dry-run reconcile plan (add/change/delete/invalid).
//!
//! The mutating verbs are declared but stubbed so the surface shape is visible:
//!
//!   - `configsource.apply`  — write the plan to the live store (NOT YET).
//!   - `configsource.pull`   — fetch/refresh the checkout (NOT YET).
//!   - `configsource.push`   — PR-writeback of live rows to git (NOT YET).
//!   - `configsource.sync`   — pull → diff → apply → push in one shot (NOT YET).
//!
//! Schema source: the daemon's per-noun config-schema registry
//! (`db::config_store::list_schemas`), populated in-process as each domain
//! loads — the live, plugin-aware schema surface. We ALSO build the served
//! OpenAPI spec (`dispatch::openapi::inject_*`, which folds in the live unit
//! catalog) to report liveness. We refuse to reconcile against an empty schema
//! set — the documented cold-daemon failure mode (§ liveness).

// Config-row payloads and per-noun schemas are free-form upstream (shape known
// only at runtime, per noun/plugin) — modelled as `serde_json::Value` on
// purpose, mirroring `db::config_store`. See reconcile.rs for the rationale.
#![allow(clippy::disallowed_types)]

use std::path::{Path, PathBuf};

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::reconcile::{self, DiffPlan, LiveRow, RepoRow, SchemaIndex};

// ── Args / Output ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct StatusArgs {
    /// Path to the meerkat checkout root (the dir containing `config/`).
    #[arg(long)]
    pub repo: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StatusOutput {
    /// Whether the daemon is live enough to reconcile (schema registry present).
    pub daemon_live: bool,
    /// Human-readable liveness verdict.
    pub liveness: String,
    /// Resolved checkout root, if one was given.
    pub repo_path: Option<String>,
    /// Whether `<repo>/config/` exists.
    pub repo_present: bool,
    /// Rows parsed out of the checkout (0 when absent).
    pub repo_rows: usize,
    /// Rows currently in the live config store.
    pub live_config_rows: usize,
    /// Nouns with a registered schema in the live daemon.
    pub schemas_registered: usize,
    /// Live plugin-driven unit ops (reflects loaded providers).
    pub live_unit_ops: usize,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct DiffArgs {
    /// Path to the meerkat checkout root (the dir containing `config/`).
    #[arg(long)]
    pub repo: Option<String>,
    /// Limit the diff to a single host_owner (`config/<host>/`).
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DiffOutput {
    /// The reconcile plan. Nothing here is executed — read-only.
    #[serde(flatten)]
    pub plan: DiffPlan,
    /// host_owners considered (directory names under `config/`).
    pub hosts: Vec<String>,
    /// Rows parsed from the checkout.
    pub repo_rows: usize,
    /// Rows read from the live config store.
    pub live_config_rows: usize,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct NotImplementedArgs {}

/// Output shape for the not-yet-implemented verbs. These verbs always error;
/// the typed struct keeps the OpenAPI surface free of opaque JSON.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NotImplementedOutput {
    pub implemented: bool,
    pub detail: String,
}

// ── Implemented verbs ────────────────────────────────────────────────────────

/// Report ConfigSource readiness: daemon liveness, checkout presence, and the
/// repo-vs-live row counts. Read-only; never touches the store.
#[orca_tool(domain = "configsource", verb = "status")]
async fn configsource_status(
    args: StatusArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StatusOutput> {
    let conn = db::open_default()?;
    let index = live_schema_index(&conn)?;
    let live_rows = db::config_store::list(&conn, None, None)?;
    let unit_ops = dispatch::unit_surface::unit_ops().len();

    let (daemon_live, liveness) = liveness_verdict(&index, unit_ops);

    let repo_path = args.repo.clone();
    let (repo_present, repo_rows) = match args.repo.as_deref() {
        Some(root) => {
            let config_dir = Path::new(root).join("config");
            if config_dir.is_dir() {
                let rows = walk_checkout(&config_dir, None)?;
                (true, rows.len())
            } else {
                (false, 0)
            }
        }
        None => (false, 0),
    };

    Ok(StatusOutput {
        daemon_live,
        liveness,
        repo_path,
        repo_present,
        repo_rows,
        live_config_rows: live_rows.len(),
        schemas_registered: index.len(),
        live_unit_ops: unit_ops,
    })
}

/// Dry-run reconcile: parse `config/<host>/*.toml`, validate each row against
/// its live noun schema (Draft 2020-12), and diff against the live config store.
/// Returns `{to_add, to_change, to_delete, schema_invalid}`. Mutates NOTHING —
/// deletes are reported, never executed.
#[orca_tool(domain = "configsource", verb = "diff")]
async fn configsource_diff(args: DiffArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<DiffOutput> {
    let conn = db::open_default()?;
    let index = live_schema_index(&conn)?;
    let unit_ops = dispatch::unit_surface::unit_ops().len();

    // Assert daemon-liveness BEFORE reconciling. Validating against an empty /
    // core-only schema set would wrongly reject every plugin-owned row — the #1
    // documented risk. Fail loud instead.
    let (daemon_live, liveness) = liveness_verdict(&index, unit_ops);
    if !daemon_live {
        anyhow::bail!(
            "refusing to reconcile: {liveness}. The config-schema registry is empty, \
             which means domains/plugins have not registered their schemas yet. \
             Run against a fully-started daemon."
        );
    }

    let root = args
        .repo
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--repo <meerkat-checkout> is required"))?;
    let config_dir = Path::new(root).join("config");
    anyhow::ensure!(
        config_dir.is_dir(),
        "no config/ directory at checkout `{root}`"
    );

    let repo_rows = walk_checkout(&config_dir, args.host.as_deref())?;
    let live = db::config_store::list(&conn, None, args.host.as_deref())?;
    let live_rows: Vec<LiveRow> = live
        .iter()
        .map(to_live_row)
        .collect::<anyhow::Result<_>>()?;

    // Hosts = the config/<host>/ dirs we actually parsed rows for.
    let mut hosts: Vec<String> = repo_rows
        .iter()
        .map(|r| r.host_owner.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if let Some(h) = &args.host
        && !hosts.contains(h)
    {
        hosts.push(h.clone());
    }

    let mut plan = DiffPlan::default();
    for host in &hosts {
        let host_plan = reconcile::compute_diff(host, &repo_rows, &live_rows, &index);
        plan.to_add.extend(host_plan.to_add);
        plan.to_change.extend(host_plan.to_change);
        plan.to_delete.extend(host_plan.to_delete);
        plan.schema_invalid.extend(host_plan.schema_invalid);
    }

    Ok(DiffOutput {
        plan,
        hosts,
        repo_rows: repo_rows.len(),
        live_config_rows: live_rows.len(),
    })
}

// ── Stubbed verbs (surface shape only) ───────────────────────────────────────

/// [STUB] Write the reconcile plan to the live config store. Not implemented in
/// the read-only slice — the mutation path (create/update/delete under
/// `updated_by = "configsource"`) is a follow-up.
#[orca_tool(domain = "configsource", verb = "apply")]
async fn configsource_apply(
    _args: NotImplementedArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotImplementedOutput> {
    anyhow::bail!(
        "configsource.apply is not yet implemented (read-only slice; mutation is a follow-up)"
    )
}

/// [STUB] Fetch/refresh the meerkat checkout (git pull). Not implemented in the
/// read-only slice.
#[orca_tool(domain = "configsource", verb = "pull")]
async fn configsource_pull(
    _args: NotImplementedArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotImplementedOutput> {
    anyhow::bail!("configsource.pull is not yet implemented (read-only slice)")
}

/// [STUB] PR-writeback of live rows back to git. Not implemented in the
/// read-only slice.
#[orca_tool(domain = "configsource", verb = "push")]
async fn configsource_push(
    _args: NotImplementedArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotImplementedOutput> {
    anyhow::bail!(
        "configsource.push is not yet implemented (read-only slice; PR-writeback is a follow-up)"
    )
}

/// [STUB] One-shot pull → diff → apply → push. Not implemented in the read-only
/// slice.
#[orca_tool(domain = "configsource", verb = "sync")]
async fn configsource_sync(
    _args: NotImplementedArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<NotImplementedOutput> {
    anyhow::bail!("configsource.sync is not yet implemented (read-only slice)")
}

// ── Native support ───────────────────────────────────────────────────────────

/// Build the per-noun schema index from the live daemon's config-schema
/// registry. This is the in-process, plugin-aware schema surface.
fn live_schema_index(conn: &db::Conn) -> anyhow::Result<SchemaIndex> {
    let mut index = SchemaIndex::new();
    for schema in db::config_store::list_schemas(conn)? {
        let value: Value = serde_json::from_str(&schema.schema_json).map_err(|e| {
            anyhow::anyhow!(
                "registered schema for noun `{}` is not valid JSON: {e}",
                schema.noun
            )
        })?;
        index.insert(schema.noun, value);
    }
    Ok(index)
}

/// Liveness verdict. The daemon is "live enough to reconcile" once its
/// config-schema registry is populated — proof that domains/plugins have
/// loaded. `unit_ops` is reported alongside as the live plugin-catalog signal.
fn liveness_verdict(index: &SchemaIndex, unit_ops: usize) -> (bool, String) {
    if index.is_empty() {
        (
            false,
            format!("daemon appears COLD: 0 config schemas registered ({unit_ops} live unit ops)"),
        )
    } else {
        (
            true,
            format!(
                "live: {} config schemas registered, {unit_ops} live unit ops",
                index.len()
            ),
        )
    }
}

fn to_live_row(r: &db::config_store::ConfigRow) -> anyhow::Result<LiveRow> {
    let json: Value = serde_json::from_str(&r.json)
        .map_err(|e| anyhow::anyhow!("live row {}/{} has invalid JSON: {e}", r.noun, r.name))?;
    Ok(LiveRow {
        host_owner: r.host_owner.clone(),
        noun: r.noun.clone(),
        name: r.name.clone(),
        json,
        is_replica: r.is_replica,
    })
}

/// Walk `config/<host>/*.toml`, parsing every row. `host_filter` limits the walk
/// to one host dir. Directories whose name starts with `_` are meta (e.g.
/// `_fleet`) and skipped — the dir name is the authoritative `host_owner`, and a
/// meta dir is not a host.
fn walk_checkout(config_dir: &Path, host_filter: Option<&str>) -> anyhow::Result<Vec<RepoRow>> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(config_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let host = entry.file_name().to_string_lossy().into_owned();
        if host.starts_with('_') {
            continue;
        }
        if let Some(want) = host_filter
            && host != want
        {
            continue;
        }
        rows.extend(parse_host_dir(&entry.path(), &host)?);
    }
    rows.sort_by(|a, b| {
        (a.host_owner.as_str(), a.noun.as_str(), a.name.as_str()).cmp(&(
            b.host_owner.as_str(),
            b.noun.as_str(),
            b.name.as_str(),
        ))
    });
    Ok(rows)
}

fn parse_host_dir(dir: &Path, host: &str) -> anyhow::Result<Vec<RepoRow>> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path: PathBuf = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let src = std::fs::read_to_string(&path)?;
        let parsed = reconcile::parse_host_config(host, &src)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        rows.extend(parsed);
    }
    Ok(rows)
}
