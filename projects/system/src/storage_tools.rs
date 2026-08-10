//! Generic storage tool surface.
//!
//! orca does not care *what kind* of storage a provider is — NFS, SMB,
//! Proxmox-managed disk — only that it has access to storage and what that
//! storage can do. These verbs iterate the process-global `storage` registry
//! ([`plugin_toolkit::storage`]) that each adapter plugin registers itself
//! against at bootstrap, rather than naming any backend by type:
//!
//! * `storage.list`             — every registered provider + its capabilities
//! * `storage.detail{view}`     — capacity/usage for a volume (`view=usage`)
//! * `storage.share.list{live}` — canonical share definitions, or (`live=true`) a live enumeration across registered backends
//! * `storage.mount.update{action}` — the mount imperatives folded onto one verb:
//!   `action=apply` renders the declared `managed_mounts` into autofs + reload;
//!   `action=unmount` unmounts a target on a named backend; `action=recover`
//!   self-heals stale autofs mounts; (absent) edits a `mounts` placement row.
//!
//! The CRUD verbs of `storage.mount.*` and `storage.share.*` are macro-generated
//! (see `mounts.rs` / `shares.rs`); the collision-free `update` / `list` above and
//! the imperatives here are hand-written and dispatch on `action` / `live`.
//!
//! Dispatched through the single daemon handler so CLI / REST / MCP / UI share
//! one path ([[feedback-cli-api-mcp-one-path]]).

use derive::orca_tool;
use plugin_toolkit::storage::{self, Capability, ExportEntry, MountOutcome, Provider, Usage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── mount placement views (reference-object pattern) ─────────────────────────

/// A reference to another table's row — a field that points at `<table>.id`
/// carries the id nested under the table's name, never a bare `shareId`/`hostId`
/// ([[no-top-level-urls-use-addresses-array]] sibling: reference-object rule).
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MountRef {
    pub id: String,
}

/// A `mounts` placement projected for the API: the true PK `id`, the per-host
/// `name` label, and `share` / `host` as nested id references.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MountView {
    pub id: String,
    pub name: String,
    pub share: MountRef,
    pub host: MountRef,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remount_policy: Option<plugin_toolkit::storage::RemountPolicy>,
    /// Last-known liveness, written by the convergence tick — the STORED value,
    /// never a live probe (the read path takes no fan-out).
    pub health: plugin_toolkit::storage::Health,
    /// Source (`host:/export`) the tick last mounted from, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_route: Option<String>,
    /// Comma-joined live `-o` option tokens the kernel reports for this mount —
    /// the STORED value the tick observed, so an operator can see hard-vs-soft
    /// per host without SSHing in. Absent when nothing is mounted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_options: Option<String>,
    /// Whether the live options diverge from the share's rendered options — the
    /// operator-visible answer to "has this host drifted (still `hard`)?".
    pub drift: bool,
    pub routes: plugin_toolkit::route::Routes,
    pub enabled: bool,
}

/// Project a placement row onto its API view.
fn mount_view(row: &crate::mounts::EndpointRow) -> MountView {
    MountView {
        id: row.id.clone(),
        name: row.name.clone(),
        share: MountRef {
            id: row.share_id.clone(),
        },
        host: MountRef {
            id: row.host.clone(),
        },
        target: row.target.clone(),
        remount_policy: row.remount_policy.clone(),
        health: row.health,
        active_route: row.active_route.clone(),
        active_options: row.active_options.clone(),
        drift: row.drift,
        routes: row.routes.clone(),
        enabled: row.enabled,
    }
}

// ── list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageListArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageListOutput {
    pub providers: Vec<Provider>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total providers across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Every storage backend registered with this daemon, with the capabilities
/// each advertises. Empty before any storage adapter has bootstrapped.
#[orca_tool(domain = "storage", verb = "list")]
async fn storage_list(
    args: StorageListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageListOutput> {
    let mut providers = storage::providers();
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(providers, &params);
    Ok(StorageListOutput {
        providers: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

// ── share.list ───────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageShareListArgs {
    /// Live-enumerate shares/volumes off the registered backends instead of
    /// reading the replicated `shares` table. Default false (table read).
    #[arg(long)]
    pub live: Option<bool>,
    /// `live` only: restrict discovery to a single backend by provider name.
    /// Empty = all backends that advertise the `list` capability.
    #[arg(long)]
    pub provider: Option<String>,
    /// Table read only: max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Table read only: opaque cursor from a previous page's `nextCursor`.
    #[arg(long)]
    pub cursor: Option<String>,
}

/// A share/volume tagged with the backend that exposes it. Flat projection of
/// [`plugin_toolkit::storage::Share`] so consumers don't depend on the domain type.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ShareRow {
    pub provider: String,
    pub id: String,
    pub source: String,
    pub target: Option<String>,
    pub fstype: String,
    pub mounted: bool,
    /// Configured ordered sources (primary → failover) for this share's `target`,
    /// joined from the declarative `managed_mounts` store. `source` above is the
    /// live/active source the backend reports; this is the authored failover
    /// order. Empty when no managed mount declares this target.
    pub configured_sources: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageSharesOutput {
    pub shares: Vec<ShareRow>,
    /// Per-backend enumeration errors (non-fatal), keyed by provider name, so a
    /// single unreachable backend doesn't blank the whole listing.
    pub errors: Vec<StorageBackendError>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageBackendError {
    pub provider: String,
    pub error: String,
}

/// A page of canonical `shares` definitions read from the replicated table.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageShareRegisteredList {
    pub shares: Vec<crate::shares::EndpointEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// `storage.share.list` result — the registered table page by default, or the
/// live cross-backend discovery when `live=true`. Untagged: consumers key off
/// the shape (`shares[].backend` vs `shares[].provider`).
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase", untagged)]
pub enum StorageShareListOutput {
    Registered(StorageShareRegisteredList),
    Live(StorageSharesOutput),
}

/// Live-enumerate shares/volumes across registered backends. Backends that don't
/// advertise `list` are skipped; per-backend failures are collected into
/// `errors` rather than failing the whole call.
async fn discover_live_shares(provider: Option<&str>) -> StorageSharesOutput {
    // Join key: mount `target` → its configured ordered sources (primary →
    // failover) from the declarative store. A live share reports only its active
    // source; the authored failover order lives in `managed_mounts`. A failed
    // store read leaves the map empty so discovery still returns the live shares.
    let configured_by_target: std::collections::HashMap<String, Vec<String>> =
        crate::managed_mounts::endpoint_db::list()
            .unwrap_or_default()
            .into_iter()
            .map(|m| {
                (
                    m.target,
                    crate::managed_mounts::ordered_sources(
                        &m.source,
                        m.failover_sources.as_deref(),
                    ),
                )
            })
            .collect();

    let mut shares = Vec::new();
    let mut errors = Vec::new();
    for b in storage::backends() {
        if let Some(want) = provider
            && b.name() != want
        {
            continue;
        }
        if !b.supports(Capability::List) {
            continue;
        }
        match b.list_shares().await {
            Ok(found) => shares.extend(found.into_iter().map(|s| {
                ShareRow {
                    provider: b.name().to_string(),
                    configured_sources: s
                        .target
                        .as_deref()
                        .and_then(|t| configured_by_target.get(t).cloned())
                        .unwrap_or_default(),
                    id: s.id,
                    source: s.source,
                    target: s.target,
                    fstype: s.fstype,
                    mounted: s.mounted,
                }
            })),
            Err(e) => errors.push(StorageBackendError {
                provider: b.name().to_string(),
                error: e.to_string(),
            }),
        }
    }
    StorageSharesOutput { shares, errors }
}

/// Canonical share definitions from the replicated `shares` table (default), or
/// — with `live=true` — a live enumeration across registered backends. The two
/// are distinct surfaces: the table is the authored source of truth; `live`
/// reflects what each backend actually exposes right now.
#[orca_tool(domain = "storage.share", verb = "list")]
async fn storage_share_list(
    args: StorageShareListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageShareListOutput> {
    if args.live.unwrap_or(false) {
        return Ok(StorageShareListOutput::Live(
            discover_live_shares(args.provider.as_deref()).await,
        ));
    }
    let mut entries: Vec<crate::shares::EndpointEntry> = crate::shares::endpoint_db::list()?
        .into_iter()
        .map(|row| crate::shares::EndpointEntry {
            name: row.name.clone(),
            id: row.id.clone(),
            backend: row.backend.clone(),
            fstype: row.fstype.clone(),
            options: row.options.clone(),
            options_rendered: row.options_rendered.clone(),
            has_credential: row.credential.is_some(),
            routes: row.routes.clone(),
            enabled: row.enabled,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(entries, &params);
    Ok(StorageShareListOutput::Registered(
        StorageShareRegisteredList {
            shares: page.items,
            next_cursor: page.next_cursor,
            total: page.total,
        },
    ))
}

// ── share.update{action} + coordinated source ops ────────────────────
// Hand-written (macro `update` skipped) so the CRUD PATCH can also dispatch the
// coordinated source operations: `drain`/`resume` hold or return a failover
// route, and `reboot_source` composes drain → source reboot → wait-healthy →
// resume. `drain`/`resume`/`reboot_source` are genuinely distinct ops (not
// context detection), so they are explicit `action` variants.

/// Project a share row onto its API entry (credential folded to `has_credential`).
fn share_entry(row: &crate::shares::EndpointRow) -> crate::shares::EndpointEntry {
    crate::shares::EndpointEntry {
        name: row.name.clone(),
        id: row.id.clone(),
        backend: row.backend.clone(),
        fstype: row.fstype.clone(),
        options: row.options.clone(),
        options_rendered: row.options_rendered.clone(),
        has_credential: row.credential.is_some(),
        routes: row.routes.clone(),
        enabled: row.enabled,
    }
}

/// The coordinated source operations folded onto `storage.share.update`.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum StorageShareAction {
    /// Hold a failover route (set `enabled = false`) so convergence fails every
    /// client off it, then lazily release this host's placements of the share.
    Drain,
    /// Return a held route (set `enabled = true`); convergence re-includes it and
    /// fails back if the policy's `return_to_primary` is set.
    Resume,
    /// Coordinated source reboot: drain the route → reboot the source host →
    /// wait until its nfsd answers again (not just TCP) → resume.
    RebootSource,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageShareUpdateArgs {
    /// The share's canonical role label (the `shares` PK).
    #[arg(long)]
    pub name: String,
    /// Coordinated source op. Omit to edit the share row (CRUD PATCH).
    #[arg(long, value_enum)]
    pub action: Option<StorageShareAction>,

    // ── CRUD row edit (action omitted) ──
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long)]
    pub backend: Option<String>,
    #[arg(long)]
    pub fstype: Option<String>,
    #[arg(long)]
    pub options: Option<String>,
    #[arg(long)]
    pub options_rendered: Option<String>,
    #[arg(long)]
    pub credential: Option<String>,
    /// Replace the reachable/failover route set. Repeatable: `--route kind=url`
    /// or a JSON object. Omit to leave routes unchanged.
    #[arg(
        long = "route",
        value_parser = plugin_toolkit::route::parse_route,
        action = clap::ArgAction::Append
    )]
    #[serde(default)]
    pub routes: Vec<plugin_toolkit::route::Route>,
    #[arg(long)]
    pub enabled: Option<bool>,

    // ── action=drain|resume|reboot_source ──
    /// The failover route to drain/return, identified by its `value` (the source
    /// host, e.g. an NFS server address). Required for the coordinated actions.
    #[arg(long)]
    pub route: Option<String>,
    /// `action=reboot_source`: mesh peer id to dispatch the reboot to. Defaults
    /// to the drained route's `value` (host).
    #[arg(long)]
    pub source_peer: Option<String>,
    /// `action=reboot_source`: the tool the source peer runs to reboot itself
    /// (dispatched over the mesh). Required — the repo exposes no single host
    /// reboot primitive, so the operator names the peer's reboot tool explicitly.
    #[arg(long)]
    pub reboot_tool: Option<String>,
    /// `action=reboot_source`: overall seconds to wait for the source's nfsd to
    /// answer again after the reboot before giving up. Defaults to 300.
    #[arg(long)]
    pub wait_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageShareEditOutput {
    pub share: crate::shares::EndpointEntry,
    pub applied: Vec<String>,
}

/// Outcome of a coordinated `drain`/`resume`/`reboot_source`.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageShareCoordOutput {
    pub share: crate::shares::EndpointEntry,
    /// The route `value` that was drained / resumed.
    pub route: String,
    /// Whether the route is currently held (drained).
    pub held: bool,
    /// `reboot_source` only: whether the source's nfsd answered again within the
    /// wait budget before resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_healthy: Option<bool>,
    /// Human-readable per-step trail.
    pub steps: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase", untagged)]
pub enum StorageShareUpdateOutput {
    Edit(StorageShareEditOutput),
    Coord(StorageShareCoordOutput),
}

/// Set the `enabled` flag on the share route whose `value` matches `route_value`,
/// persisting the change (replicated). Returns whether a route matched.
fn set_route_enabled(
    row: &mut crate::shares::EndpointRow,
    route_value: &str,
    enabled: bool,
) -> bool {
    let mut routes: Vec<plugin_toolkit::route::Route> = row.routes.iter().cloned().collect();
    let mut found = false;
    for r in &mut routes {
        if r.value == route_value {
            r.enabled = enabled;
            found = true;
        }
    }
    if found {
        row.routes = plugin_toolkit::route::Routes::from(routes);
    }
    found
}

/// Lazily release every LOCAL placement of `share_id` mounted from a source on
/// `route_value` host: `umount -l` → wait `settle_secs` → `umount -l -f`. Remote
/// placements are left to each client's convergence loop (which sees the held
/// route and fails off it). Returns the targets it released here.
async fn drain_local_placements(share_id: &str, this_host: &str, settle_secs: u32) -> Vec<String> {
    let targets: Vec<String> = crate::mounts::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.enabled && m.share_id == share_id && m.host == this_host)
        .map(|m| m.target)
        .collect();
    if targets.is_empty() {
        return targets;
    }
    // Lazy detach first (lets in-flight I/O finish), settle, then force.
    let _ = crate::autofs::run_privileged(&crate::autofs::PrivilegedOp::Unmount {
        targets: targets.clone(),
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(settle_secs as u64)).await;
    let _ = crate::autofs::run_privileged(&crate::autofs::PrivilegedOp::Unmount {
        targets: targets.clone(),
    })
    .await;
    targets
}

/// Poll `probe_source_nfs` until the source host answers RPC (nfsd live, not just
/// TCP up) or the `overall` budget elapses. The real orchestration guard: a
/// reboot is only "done" once the server serves NFS again. Returns `true` on
/// healthy, `false` on timeout.
async fn wait_source_healthy(
    host: &str,
    per_attempt: std::time::Duration,
    overall: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + overall;
    loop {
        let h = host.to_string();
        let live = tokio::task::spawn_blocking(move || {
            plugin_toolkit::storage::probe_source_nfs(&h, per_attempt)
        })
        .await
        .unwrap_or(false);
        if live {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Edit a share row (CRUD PATCH) — mirrors the macro-generated update the
/// `skip = "update"` withholds.
fn share_row_edit(args: &StorageShareUpdateArgs) -> anyhow::Result<StorageShareEditOutput> {
    let mut row = crate::shares::endpoint_db::get(&args.name)?
        .ok_or_else(|| plugin_toolkit::runtime::missing_row_error("storage.share", &args.name))?;
    let mut applied: Vec<String> = Vec::new();
    if let Some(v) = args.id.clone() {
        row.id = v;
        applied.push("id".to_string());
    }
    if let Some(v) = args.backend.clone() {
        row.backend = v;
        applied.push("backend".to_string());
    }
    if let Some(v) = args.fstype.clone() {
        row.fstype = v;
        applied.push("fstype".to_string());
    }
    if let Some(v) = args.options.clone() {
        row.options = v;
        applied.push("options".to_string());
    }
    if let Some(v) = args.options_rendered.clone() {
        row.options_rendered = v;
        applied.push("options_rendered".to_string());
    }
    if let Some(v) = args.credential.clone() {
        row.credential = Some(v);
        applied.push("credential".to_string());
    }
    if !args.routes.is_empty() {
        row.routes = plugin_toolkit::route::Routes::from(args.routes.clone());
        applied.push("routes".to_string());
    }
    if let Some(v) = args.enabled {
        row.enabled = v;
        applied.push("enabled".to_string());
    }
    if applied.is_empty() {
        anyhow::bail!("no fields to update; pass at least one flag");
    }
    let changed = crate::shares::endpoint_db::update(&row)?;
    if !changed {
        anyhow::bail!("update reported no row change for `{}`", row.name);
    }
    Ok(StorageShareEditOutput {
        share: share_entry(&row),
        applied,
    })
}

/// Drive a coordinated `drain` / `resume` / `reboot_source` on a share route.
async fn share_coord(
    args: &StorageShareUpdateArgs,
    action: StorageShareAction,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageShareCoordOutput> {
    let route_value = args
        .route
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("`route` is required for a coordinated share action"))?;
    let mut row = crate::shares::endpoint_db::get(&args.name)?
        .ok_or_else(|| plugin_toolkit::runtime::missing_row_error("storage.share", &args.name))?;
    let this_host = crate::host_identity::machine_id();
    let mut steps: Vec<String> = Vec::new();

    // Drain policy from the placements referencing this share on this host (the
    // typed remount policy owns settle/mode). Fall back to the default.
    let drain = crate::mounts::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.share_id == row.id && m.host == this_host)
        .and_then(|m| m.remount_policy)
        .map(|p| p.drain)
        .unwrap_or_default();

    match action {
        StorageShareAction::Resume => {
            if !set_route_enabled(&mut row, route_value, true) {
                anyhow::bail!(
                    "share `{}` has no route with value `{route_value}`",
                    args.name
                );
            }
            crate::shares::endpoint_db::update(&row)?;
            steps.push(format!("resumed route {route_value} (enabled=true)"));
            Ok(StorageShareCoordOutput {
                share: share_entry(&row),
                route: route_value.to_string(),
                held: false,
                source_healthy: None,
                steps,
            })
        }
        StorageShareAction::Drain => {
            if !set_route_enabled(&mut row, route_value, false) {
                anyhow::bail!(
                    "share `{}` has no route with value `{route_value}`",
                    args.name
                );
            }
            crate::shares::endpoint_db::update(&row)?;
            steps.push(format!("held route {route_value} (enabled=false)"));
            if drain.enabled {
                let released = drain_local_placements(&row.id, this_host, drain.settle_secs).await;
                steps.push(format!("released local placements: {released:?}"));
            }
            Ok(StorageShareCoordOutput {
                share: share_entry(&row),
                route: route_value.to_string(),
                held: true,
                source_healthy: None,
                steps,
            })
        }
        StorageShareAction::RebootSource => {
            let reboot_tool = args.reboot_tool.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "`reboot_tool` is required for action=reboot_source (the peer's reboot tool)"
                )
            })?;
            let peer = args.source_peer.as_deref().unwrap_or(route_value);
            // 1. Drain: hold the route + release local placements.
            set_route_enabled(&mut row, route_value, false);
            crate::shares::endpoint_db::update(&row)?;
            steps.push(format!("drained route {route_value}"));
            if drain.enabled {
                let released = drain_local_placements(&row.id, this_host, drain.settle_secs).await;
                steps.push(format!("released local placements: {released:?}"));
            }
            // 2. Reboot the source host over the mesh.
            let exec = ctx
                .service::<std::sync::Arc<dyn contract::RemoteExec>>()
                .map_err(|_| {
                    anyhow::anyhow!("no RemoteExec transport available to reboot the source")
                })?;
            exec.exec(peer, reboot_tool, serde_json::json!({}), None, None)
                .await
                .map_err(|e| anyhow::anyhow!("dispatching reboot to `{peer}`: {e}"))?;
            steps.push(format!("dispatched `{reboot_tool}` to peer `{peer}`"));
            // 3. Wait until the source's nfsd answers again (the real guard).
            let overall = std::time::Duration::from_secs(args.wait_secs.unwrap_or(300));
            let healthy =
                wait_source_healthy(route_value, std::time::Duration::from_secs(5), overall).await;
            steps.push(format!("source nfsd healthy after reboot: {healthy}"));
            // 4. Resume: return the route so convergence fails back.
            let mut row = crate::shares::endpoint_db::get(&args.name)?.ok_or_else(|| {
                plugin_toolkit::runtime::missing_row_error("storage.share", &args.name)
            })?;
            set_route_enabled(&mut row, route_value, true);
            crate::shares::endpoint_db::update(&row)?;
            steps.push(format!("resumed route {route_value}"));
            Ok(StorageShareCoordOutput {
                share: share_entry(&row),
                route: route_value.to_string(),
                held: false,
                source_healthy: Some(healthy),
                steps,
            })
        }
    }
}

/// Edit a share row, or drive a coordinated source op. `action` omitted → CRUD
/// PATCH; `drain` / `resume` / `reboot_source` → the coordinated orchestration.
#[orca_tool(domain = "storage.share", verb = "update")]
async fn storage_share_update(
    args: StorageShareUpdateArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageShareUpdateOutput> {
    match args.action {
        None => Ok(StorageShareUpdateOutput::Edit(share_row_edit(&args)?)),
        Some(action) => Ok(StorageShareUpdateOutput::Coord(
            share_coord(&args, action, ctx).await?,
        )),
    }
}

// ── exports ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageExportsArgs {
    /// Restrict enumeration to a single backend by provider name. Empty = all
    /// backends that advertise the `exports` capability.
    #[arg(long)]
    pub provider: Option<String>,
}

/// An export a host serves, tagged with the backend that publishes it. Flat
/// projection of [`plugin_toolkit::storage::ExportEntry`].
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExportRow {
    pub provider: String,
    pub path: String,
    pub allowed_clients: Vec<String>,
    pub options: Vec<String>,
    pub fsid: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageExportsOutput {
    pub exports: Vec<ExportRow>,
    /// Per-backend enumeration errors (non-fatal), keyed by provider name, so a
    /// single unreachable backend doesn't blank the whole listing.
    pub errors: Vec<StorageBackendError>,
}

/// What this host *serves* to the network — the NFS/SMB server exports each
/// registered backend publishes, distinct from what the host mounts
/// (`storage.share.list`). Backends that don't advertise the `exports`
/// capability are skipped; per-backend failures are collected into `errors`
/// rather than failing the whole call. Empty until a backend implements
/// `list_exports` (nfs reading `/etc/exports` / `showmount -e`, unraid reading
/// share config).
#[orca_tool(domain = "storage", verb = "exports")]
async fn storage_exports(
    args: StorageExportsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageExportsOutput> {
    let mut exports = Vec::new();
    let mut errors = Vec::new();
    for b in storage::backends() {
        if let Some(want) = args.provider.as_deref()
            && b.name() != want
        {
            continue;
        }
        if !b.supports(Capability::Exports) {
            continue;
        }
        match b.list_exports().await {
            Ok(found) => exports.extend(found.into_iter().map(|e: ExportEntry| ExportRow {
                provider: b.name().to_string(),
                path: e.path,
                allowed_clients: e.allowed_clients,
                options: e.options,
                fsid: e.fsid,
            })),
            Err(e) => errors.push(StorageBackendError {
                provider: b.name().to_string(),
                error: e.to_string(),
            }),
        }
    }
    Ok(StorageExportsOutput { exports, errors })
}

// ── mount.update{action=apply} ───────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageMountOutput {
    /// Number of enabled network-share mounts rendered into the autofs map.
    pub rendered: usize,
    /// Config files that changed this run (the drift set). Empty = host already
    /// matched the declared store.
    pub changed: Vec<String>,
    /// Whether autofs was reloaded (only when something changed).
    pub reloaded: bool,
    /// Mountpoints accessed to force an immediate mount (when `trigger`).
    pub triggered: Vec<String>,
    /// Userspace-process (object-store/FUSE) mounts brought up this run, via the
    /// owning backend's helper process rather than autofs.
    pub userspace_mounted: Vec<String>,
    /// Userspace-process mounts torn down this run (disabled rows).
    pub userspace_unmounted: Vec<String>,
    /// Non-fatal errors during apply/trigger.
    pub errors: Vec<String>,
}

/// Render every enabled network-share entry in the `managed_mounts` store into
/// the orca autofs direct map and reload autofs. autofs then owns on-demand
/// mounting, idle unmount, and ordered-source (primary → failover) failover;
/// the recover sweep covers the one case autofs can't self-heal (an
/// actively-held stale hard mount). Idempotent — a run that changes nothing
/// neither rewrites files nor reloads autofs. Reached via
/// `storage.mount.update{action=apply}`.
/// Adapt one resolved [`crate::mount_converge::DesiredMount`] (a placement joined
/// to its share, with the share's ordered/enabled routes and rendered options)
/// into the [`crate::managed_mounts::ManagedMount`] render input the autofs
/// builders consume. Primary is `sources[0]`; any remaining sources become the
/// newline-joined `failover_sources` autofs lists as replicated servers. Render
/// concern only: `remount_policy` (a plan/failover concern) never shapes a map
/// line, so it is left `None`. `kind` is derived from `fstype` so object-store
/// backends (rendered by the userspace path, not autofs) are excluded from the
/// direct map exactly as before.
fn desired_to_render_mount(
    d: &crate::mount_converge::DesiredMount,
) -> crate::managed_mounts::ManagedMount {
    let (source, failover_sources) = match d.sources.split_first() {
        Some((first, rest)) if !rest.is_empty() => (first.clone(), Some(rest.join("\n"))),
        Some((first, _)) => (first.clone(), None),
        None => (String::new(), None),
    };
    let kind =
        if d.fstype.starts_with("nfs") || d.fstype.contains("cifs") || d.fstype.contains("smb") {
            "network_share"
        } else {
            "object"
        };
    crate::managed_mounts::ManagedMount {
        name: d.target.clone(),
        backend: d.backend.clone(),
        kind: kind.into(),
        source,
        failover_sources,
        target: d.target.clone(),
        fstype: d.fstype.clone(),
        options: (!d.options.is_empty()).then(|| d.options.clone()),
        credential: d.credential.clone(),
        remount_policy: None,
        routes: Default::default(),
        enabled: true,
    }
}

async fn mount_apply(trigger: Option<bool>) -> anyhow::Result<StorageMountOutput> {
    // Source the render set from the replicated `mounts` placements joined to
    // their `shares` (the share's ordered, enabled routes — primary first,
    // failover after — with backend-rendered options) for THIS host. The retired
    // per-host-local `managed_mounts` table is unreplicated and no longer
    // authored, so reading it rendered a header-only (0-line) map that wiped
    // `/etc/auto.orca`; resolving from `mounts`⋈`shares`⋈`routes` is what lets
    // `apply` emit the declared primary/failover map orca actually owns.
    let host = crate::host_identity::machine_id();
    let desired = crate::mount_converge::desired_for_host(host)?;
    let mounts: Vec<crate::managed_mounts::ManagedMount> =
        desired.iter().map(desired_to_render_mount).collect();

    // Pin each mount to its PRIMARY route (`sources[0]`) as the elected single
    // autofs location. Rendering all ordered sources on one line lets autofs treat
    // them as replicated servers and pick by lowest latency — which is NOT the
    // declared primary when a failover replica is co-located (e.g. maple runs as a
    // VM on frigg, so its RTT beats willow and autofs would always mount maple).
    // A single elected location is deterministic: orca owns the primary, and the
    // convergence loop (source-health election in `mount_converge::tick`) is what
    // re-points to a failover when the primary is actually unhealthy.
    let elected: std::collections::HashMap<String, String> = desired
        .iter()
        .filter_map(|d| d.sources.first().map(|s| (d.target.clone(), s.clone())))
        .collect();
    let rendered = crate::autofs::render_map_elected(&mounts, &elected)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .count();

    // Kernel-mount (nfs/smb) path. autofs owns these; the elected renderer filters
    // to `kind == "network_share"`, and userspace-process mounts (object stores)
    // never enter the map, so this call is unaffected by them.
    let applied = crate::autofs::apply_elected(&mounts, &elected).await;

    let mut triggered = Vec::new();
    let mut errors = applied.errors;
    if trigger.unwrap_or(true) {
        let targets: Vec<String> = mounts
            .iter()
            .filter(|m| m.enabled && m.kind == "network_share")
            .map(|m| m.target.clone())
            .collect();
        errors.extend(crate::autofs::trigger(&targets).await);
        triggered = targets;
    }

    // Userspace-process (object-store/FUSE) path — driven through the backend's
    // helper, NOT autofs. Branches on the backend's `mount_style` per row; a
    // kernel-mount row is skipped here (and vice-versa above), so the two paths
    // never overlap.
    let usp = crate::userspace_mounts::reconcile(&mounts).await;
    errors.extend(usp.errors);

    Ok(StorageMountOutput {
        rendered,
        changed: applied.changed,
        reloaded: applied.reloaded,
        triggered,
        userspace_mounted: usp.mounted,
        userspace_unmounted: usp.unmounted,
        errors,
    })
}

// ── recover (shared backend-routed helper) ───────────────────────────

/// Merged outcome of a backend-routed recovery sweep. Mirrors
/// [`crate::autofs::RecoverOutcome`] but additionally carries the
/// declared-but-absent remount vecs a [`storage::RecoverOutcome`] reports, so a
/// plugin's consumer-aware sweep (nfs's `consumer:` / `consumer-skipped-*`
/// tagged entries fold into `recovered` / `still_stale`) is surfaced losslessly.
#[derive(Debug, Default, Clone)]
pub struct MergedRecover {
    pub recovered: Vec<String>,
    pub still_stale: Vec<String>,
    pub healthy: Vec<String>,
    pub remounted: Vec<String>,
    pub still_missing: Vec<String>,
    pub errors: Vec<String>,
    pub no_stale_found: bool,
}

/// The routing decision computed by [`plan_recovery`]: which recover-capable
/// backend gets which targets, and which targets fall back to autofs. Pure data
/// (no I/O) so the target→backend routing is unit-testable without a live
/// registry or touching real mounts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// `(backend_name, targets)` for each recover-capable backend, sorted by name.
    pub backend_calls: Vec<(String, Vec<String>)>,
    /// Targets owned by an unknown or non-recover-capable backend — autofs fallback.
    pub fallback_targets: Vec<String>,
}

/// Group `(backend, target)` entries by their declared backend and split into
/// recover-capable backend invocations vs autofs fallback.
/// `is_recover_capable(name)` reports whether a registered backend of that name
/// advertises `RecoverStale`.
///
/// Attribution is exact: each entry names its owning backend, so every backend is
/// called with only its own targets (no need to have backends no-op on foreign
/// targets). Generic over the entry source so both the legacy `managed_mounts`
/// path and the native converge desired set (shares⋈mounts⋈routes) feed it.
pub fn plan_recovery<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    is_recover_capable: impl Fn(&str) -> bool,
) -> RecoveryPlan {
    use std::collections::BTreeMap;

    let mut by_backend: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (backend, target) in entries {
        by_backend
            .entry(backend.to_string())
            .or_default()
            .push(target.to_string());
    }

    let mut plan = RecoveryPlan::default();
    for (name, targets) in by_backend {
        if is_recover_capable(&name) {
            plan.backend_calls.push((name, targets));
        } else {
            plan.fallback_targets.extend(targets);
        }
    }
    plan
}

/// Whether a registered backend of `name` advertises [`Capability::RecoverStale`].
fn backend_recover_capable(name: &str) -> bool {
    storage::backend(name)
        .map(|b| b.capabilities().contains(&Capability::RecoverStale))
        .unwrap_or(false)
}

/// Run each recover-capable backend against exactly its own `(backend, target)`
/// entries and merge the outcomes — NO autofs fallback. This is the
/// consumer-stale (ESTALE-inside-a-guest) heal the native convergence loop drives
/// against its desired set: the one heal a host-mount lifecycle cannot do itself,
/// because the host mount is healthy while a container pins a stale NFS
/// superblock. Targets owned by a non-recover-capable backend are simply not
/// swept here (converge owns host-mount staleness directly; there is nothing an
/// autofs fallback could add for those).
pub async fn recover_backends_only<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    timeout: std::time::Duration,
) -> MergedRecover {
    let plan = plan_recovery(entries, backend_recover_capable);
    let mut merged = MergedRecover::default();
    for (backend_name, targets) in plan.backend_calls {
        // Guaranteed present + recover-capable by `plan_recovery`.
        let b = match storage::backend(&backend_name) {
            Some(b) => b,
            None => continue,
        };
        match b.recover_stale(&targets, timeout).await {
            Ok(out) => {
                merged.recovered.extend(out.recovered);
                merged.still_stale.extend(out.still_stale);
                merged.remounted.extend(out.remounted);
                merged.still_missing.extend(out.still_missing);
                merged.errors.extend(out.errors);
            }
            Err(e) => merged
                .errors
                .push(format!("backend `{backend_name}` recover_stale: {e}")),
        }
    }
    merged.no_stale_found = merged.recovered.is_empty()
        && merged.still_stale.is_empty()
        && merged.remounted.is_empty()
        && merged.still_missing.is_empty();
    merged
}

/// Route stale-mount recovery through registered storage backends, with an
/// autofs fallback for any target no recover-capable backend owns.
///
/// Each managed mount names its owning backend ([`ManagedMount::backend`]); we
/// group the targets by that name. For every registered backend that advertises
/// [`Capability::RecoverStale`] we invoke `recover_stale(watch, timeout)` with
/// exactly the targets attributed to it — so the nfs plugin's consumer-aware
/// bind-mount self-heal (host-healthy + consumer-stale ESTALE guard, restart of
/// containers pinning a stale superblock) actually runs. Out-of-process plugins
/// are reached transparently via the storage FFI proxy.
///
/// Targets whose backend is unknown or is not recover-capable fall back to
/// [`crate::autofs::recover`] — preserving today's behavior exactly for hosts
/// with no recover-capable backend registered.
///
/// Core never restarts containers itself: the consumer-restart path lives
/// entirely inside the plugin behind its own guard. Core's only job is to call
/// the backend.
pub async fn recover_via_backends(
    mounts: &[crate::managed_mounts::ManagedMount],
    timeout: std::time::Duration,
) -> MergedRecover {
    // Recover-capable backends first (the consumer-aware bind-mount self-heal)…
    let mut merged = recover_backends_only(
        mounts
            .iter()
            .map(|m| (m.backend.as_str(), m.target.as_str())),
        timeout,
    )
    .await;

    // …then the autofs fallback for any target whose backend is unknown or not
    // recover-capable — preserving today's behavior exactly for hosts with no
    // recover-capable backend registered.
    let fallback_targets: Vec<String> = mounts
        .iter()
        .filter(|m| !backend_recover_capable(&m.backend))
        .map(|m| m.target.clone())
        .collect();
    if !fallback_targets.is_empty() {
        let r = crate::autofs::recover(&fallback_targets, timeout).await;
        merged.recovered.extend(r.recovered);
        merged.still_stale.extend(r.still_stale);
        merged.healthy.extend(r.healthy);
        merged.errors.extend(r.errors);
    }

    merged.no_stale_found = merged.recovered.is_empty()
        && merged.still_stale.is_empty()
        && merged.remounted.is_empty()
        && merged.still_missing.is_empty();
    merged
}

// ── mount.update{action=recover} ─────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageRecoverOutput {
    pub recovered: Vec<String>,
    pub still_stale: Vec<String>,
    pub healthy: Vec<String>,
    pub errors: Vec<String>,
    pub no_stale_found: bool,
}

/// Self-heal stale autofs mounts across the declared network shares — the one
/// failure mode autofs can't recover itself (an actively-held stale `hard`
/// mount). Probes each declared target; a stale one is force-released and
/// re-accessed so autofs remounts + fails over to the next ordered source.
/// This is what the periodic self-heal schedule invokes per host. Reached via
/// `storage.mount.update{action=recover}`.
async fn mount_recover(health_timeout_secs: Option<u64>) -> anyhow::Result<StorageRecoverOutput> {
    let mounts: Vec<crate::managed_mounts::ManagedMount> =
        crate::managed_mounts::endpoint_db::list()?
            .into_iter()
            .filter(|m| m.enabled && m.kind == "network_share")
            .collect();

    let timeout = std::time::Duration::from_secs(health_timeout_secs.unwrap_or(5));
    let mut r = recover_via_backends(&mounts, timeout).await;

    // Fold the declared-but-absent remount vecs (populated by consumer-aware
    // backends) into the flat recovered/still_stale surface this tool reports.
    r.recovered.append(&mut r.remounted);
    r.still_stale.append(&mut r.still_missing);

    Ok(StorageRecoverOutput {
        recovered: r.recovered,
        still_stale: r.still_stale,
        healthy: r.healthy,
        errors: r.errors,
        no_stale_found: r.no_stale_found,
    })
}

// ── mount.update{action=unmount} ─────────────────────────────────────

/// Unmount a target on a named backend. Errors if the provider is unknown or
/// does not advertise the `unmount` capability. Reached via
/// `storage.mount.update{action=unmount}`.
async fn mount_unmount(provider: &str, target: &str) -> anyhow::Result<MountOutcome> {
    let b = storage::backend(provider)
        .ok_or_else(|| anyhow::anyhow!("no storage backend named `{provider}`"))?;
    if !b.supports(Capability::Unmount) {
        anyhow::bail!("backend `{provider}` does not support unmount");
    }
    Ok(b.unmount(target).await?)
}

// ── mount.update (dispatcher) ────────────────────────────────────────

/// The imperative mount actions, folded onto `storage.mount.update`.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum StorageMountAction {
    /// Render the declared `managed_mounts` into the autofs map and reload.
    Apply,
    /// Unmount `target` on the named `provider` backend.
    Unmount,
    /// Self-heal stale autofs mounts across the declared network shares.
    Recover,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageMountUpdateArgs {
    /// Imperative action. Omit to edit the `mounts` placement row (CRUD update).
    #[arg(long, value_enum)]
    pub action: Option<StorageMountAction>,

    // ── CRUD row edit (action omitted) — the `mounts` placement, keyed by `id` ──
    /// Placement uuidv7 `id` (the row PK) to edit.
    #[arg(long)]
    pub id: Option<String>,
    /// New per-host `name` label for this placement (unique per `host`).
    #[arg(long)]
    pub name: Option<String>,
    /// New `shares.id` this placement mounts.
    #[arg(long)]
    pub share_id: Option<String>,
    /// New target host (peer id) for this placement.
    #[arg(long)]
    pub host: Option<String>,
    /// New absolute mountpoint on `host`; also the `action=unmount` target.
    #[arg(long)]
    pub target: Option<String>,
    /// New serialized remount policy for this placement.
    #[arg(long)]
    pub remount_policy: Option<String>,
    /// Enable/disable this placement.
    #[arg(long)]
    pub enabled: Option<bool>,

    // ── action=apply ──
    /// `action=apply`: immediately trigger each declared mountpoint after
    /// rendering so shares come up now. Defaults to true.
    #[arg(long)]
    pub trigger: Option<bool>,

    // ── action=unmount ──
    /// `action=unmount`: backend provider name (e.g. `nfs`, `smb`).
    #[arg(long)]
    pub provider: Option<String>,

    // ── action=recover ──
    /// `action=recover`: per-target liveness-probe timeout in seconds. Defaults to 5.
    #[arg(long)]
    pub health_timeout_secs: Option<u64>,
}

/// The updated `mounts` placement returned by the CRUD branch.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageMountEditOutput {
    pub mount: MountView,
    pub applied: Vec<String>,
}

/// `storage.mount.update` result — one variant per branch. Untagged: the CRUD
/// edit returns `{endpoint, applied}`; each imperative returns its own shape.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase", untagged)]
pub enum StorageMountUpdateOutput {
    Edit(StorageMountEditOutput),
    Apply(StorageMountOutput),
    Unmount(MountOutcome),
    Recover(StorageRecoverOutput),
}

/// Parse a `--remount-policy` CLI argument (a JSON [`RemountPolicy`] object) into
/// the typed policy. An empty/blank value clears the policy (`None`); a malformed
/// object is a hard error rather than a silent default, so a typo surfaces at
/// author time.
fn parse_remount_policy_arg(
    v: &str,
) -> anyhow::Result<Option<plugin_toolkit::storage::RemountPolicy>> {
    let t = v.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let policy = serde_json::from_str::<plugin_toolkit::storage::RemountPolicy>(t)
        .map_err(|e| anyhow::anyhow!("invalid remount policy JSON: {e}"))?;
    Ok(Some(policy))
}

/// Edit a `mounts` placement row (PATCH semantics — must already exist). Mirrors
/// the macro-generated CRUD update the `skip = "update"` withholds so this one
/// canonical `storage.mount.update` can also dispatch the imperatives.
fn mount_row_edit(args: &StorageMountUpdateArgs) -> anyhow::Result<StorageMountEditOutput> {
    let id = args
        .id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("`id` is required to edit a mount placement"))?;
    let mut row = crate::mounts::endpoint_db::get_by_id(id)?
        .ok_or_else(|| plugin_toolkit::runtime::missing_row_error("storage.mount", id))?;
    let mut applied: Vec<String> = Vec::new();
    if let Some(v) = args.name.clone() {
        row.name = v;
        applied.push("name".to_string());
    }
    if let Some(v) = args.share_id.clone() {
        row.share_id = v;
        applied.push("share_id".to_string());
    }
    if let Some(v) = args.host.clone() {
        row.host = v;
        applied.push("host".to_string());
    }
    if let Some(v) = args.target.clone() {
        row.target = v;
        applied.push("target".to_string());
    }
    if let Some(v) = args.remount_policy.clone() {
        row.remount_policy = parse_remount_policy_arg(&v)?;
        applied.push("remount_policy".to_string());
    }
    if let Some(v) = args.enabled {
        row.enabled = v;
        applied.push("enabled".to_string());
    }
    if applied.is_empty() {
        anyhow::bail!("no fields to update; pass at least one flag");
    }
    let changed = crate::mounts::endpoint_db::update(&row)?;
    if !changed {
        anyhow::bail!("update reported no row change for `{}`", row.id);
    }
    Ok(StorageMountEditOutput {
        mount: mount_view(&row),
        applied,
    })
}

/// Edit a mount placement or drive one of the mount imperatives. `action`
/// omitted → PATCH the `mounts` placement row (CRUD); `apply` / `unmount` /
/// `recover` → the autofs-backed imperatives, byte-for-byte unchanged.
#[orca_tool(domain = "storage.mount", verb = "update")]
async fn storage_mount_update(
    args: StorageMountUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageMountUpdateOutput> {
    match args.action {
        None => Ok(StorageMountUpdateOutput::Edit(mount_row_edit(&args)?)),
        Some(StorageMountAction::Apply) => Ok(StorageMountUpdateOutput::Apply(
            mount_apply(args.trigger).await?,
        )),
        Some(StorageMountAction::Unmount) => {
            let provider = args
                .provider
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("`provider` is required for action=unmount"))?;
            let target = args
                .target
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("`target` is required for action=unmount"))?;
            Ok(StorageMountUpdateOutput::Unmount(
                mount_unmount(provider, target).await?,
            ))
        }
        Some(StorageMountAction::Recover) => Ok(StorageMountUpdateOutput::Recover(
            mount_recover(args.health_timeout_secs).await?,
        )),
    }
}

// ── mount.list / .detail / .create / .delete ─────────────────────────
// Hand-written (not macro-generated) because the `mounts` table keys by a
// uuidv7 `id` with a per-host `name` label, and the responses use the nested
// reference-object shape (`share`/`host` → `{ id }`), neither of which the
// generic `endpoint_resource` surface expresses.

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageMountListArgs {
    /// Restrict to placements targeting a single host (peer id). Empty = all.
    #[arg(long)]
    pub host: Option<String>,
}

/// Every mount placement, oldest-authored order stable by `(host, name)`, as a
/// plain array of the reference-object view. Optionally scoped to one host.
#[orca_tool(domain = "storage.mount", verb = "list")]
async fn storage_mount_list(
    args: StorageMountListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<Vec<MountView>> {
    let mut rows = crate::mounts::endpoint_db::list()?;
    rows.sort_by(|a, b| (&a.host, &a.name).cmp(&(&b.host, &b.name)));
    Ok(rows
        .iter()
        .filter(|m| args.host.as_deref().is_none_or(|h| m.host == h))
        .map(mount_view)
        .collect())
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageMountDetailArgs {
    /// Placement uuidv7 `id`. Preferred; unambiguous.
    #[arg(long)]
    pub id: Option<String>,
    /// Host (peer id) — with `--name`, resolves the per-host-unique placement.
    #[arg(long)]
    pub host: Option<String>,
    /// Per-host `name` label — with `--host`, resolves the placement.
    #[arg(long)]
    pub name: Option<String>,
}

/// A single placement by `id`, or by its per-host `(host, name)` pair.
#[orca_tool(domain = "storage.mount", verb = "detail")]
async fn storage_mount_detail(
    args: StorageMountDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MountView> {
    let row = if let Some(id) = args.id.as_deref() {
        crate::mounts::endpoint_db::get_by_id(id)?
            .ok_or_else(|| plugin_toolkit::runtime::missing_row_error("storage.mount", id))?
    } else if let (Some(host), Some(name)) = (args.host.as_deref(), args.name.as_deref()) {
        crate::mounts::endpoint_db::get_by_host_name(host, name)?
            .ok_or_else(|| anyhow::anyhow!("no mount placement `{name}` on host `{host}`"))?
    } else {
        anyhow::bail!("pass `--id`, or both `--host` and `--name`");
    };
    Ok(mount_view(&row))
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageMountCreateArgs {
    /// Per-host `name` label for the placement (unique per `host`).
    #[arg(long)]
    pub name: String,
    /// The `shares.id` this placement mounts.
    #[arg(long)]
    pub share_id: String,
    /// Target host (peer id) — the host whose convergence loop materializes it.
    #[arg(long)]
    pub host: String,
    /// Absolute mountpoint on `host`.
    #[arg(long)]
    pub target: String,
    /// Serialized remount policy (per-placement host behaviour).
    #[arg(long)]
    pub remount_policy: Option<String>,
    /// Reachable path(s), tried in order. Repeatable: `--route kind=url`.
    #[arg(
        long = "route",
        value_parser = plugin_toolkit::route::parse_route,
        action = clap::ArgAction::Append
    )]
    #[serde(default)]
    pub routes: Vec<plugin_toolkit::route::Route>,
}

/// Author a new mount placement. Errors if the host already has a placement
/// with the same `name`, mirroring `UNIQUE(host, name)`.
#[orca_tool(domain = "storage.mount", verb = "create")]
async fn storage_mount_create(
    args: StorageMountCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MountView> {
    if crate::mounts::endpoint_db::get_by_host_name(&args.host, &args.name)?.is_some() {
        anyhow::bail!(
            "mount `{}` already exists on host `{}`; use storage.mount.update",
            args.name,
            args.host
        );
    }
    let row = crate::mounts::EndpointRow {
        id: plugin_toolkit::mint_uuidv7(),
        name: args.name,
        share_id: args.share_id,
        host: args.host,
        target: args.target,
        remount_policy: args
            .remount_policy
            .as_deref()
            .map(parse_remount_policy_arg)
            .transpose()?
            .flatten(),
        health: plugin_toolkit::storage::Health::Missing,
        active_route: None,
        active_options: None,
        drift: false,
        routes: plugin_toolkit::route::Routes::from(args.routes),
        enabled: true,
    };
    crate::mounts::endpoint_db::insert(&row)?;
    Ok(mount_view(&row))
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageMountDeleteArgs {
    /// Placement uuidv7 `id` to remove.
    #[arg(long)]
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageMountDeleteOutput {
    pub id: String,
    pub changed: bool,
}

/// Remove a mount placement by `id`. Idempotent — a missing id reports
/// `changed: false`.
#[orca_tool(domain = "storage.mount", verb = "delete")]
async fn storage_mount_delete(
    args: StorageMountDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageMountDeleteOutput> {
    let changed = crate::mounts::endpoint_db::remove(&args.id)?;
    Ok(StorageMountDeleteOutput {
        id: args.id,
        changed,
    })
}

// ── detail{view=usage} ───────────────────────────────────────────────

/// The facet of a storage resource `storage.detail` reports. `usage` (capacity)
/// is the first; more views (e.g. `health`) fold in here as the enum grows.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum StorageDetailView {
    #[default]
    Usage,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageDetailArgs {
    /// Which facet to report. Defaults to `usage`.
    #[arg(long, value_enum, default_value = "usage")]
    #[serde(default)]
    pub view: StorageDetailView,
    /// Backend provider name (e.g. an object store).
    #[arg(long)]
    pub provider: String,
    /// Share/volume id to report on (`s3://bucket/prefix`, …).
    #[arg(long)]
    pub id: String,
}

/// Capacity/usage for a volume on a named backend. Errors if the provider is
/// unknown or does not advertise the `usage` capability. Object stores that
/// cannot report usage return a documented stub from the backend — this view
/// surfaces whatever the backend implements without special-casing any kind.
#[orca_tool(domain = "storage", verb = "detail")]
async fn storage_detail(
    args: StorageDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<Usage> {
    let StorageDetailView::Usage = args.view;
    let b = storage::backend(&args.provider)
        .ok_or_else(|| anyhow::anyhow!("no storage backend named `{}`", args.provider))?;
    if !b.supports(Capability::Usage) {
        anyhow::bail!("backend `{}` does not support usage", args.provider);
    }
    Ok(b.usage(&args.id).await?)
}

#[cfg(test)]
#[allow(clippy::disallowed_types)] // tests build serde_json::Value fixtures directly
mod tests {
    use super::*;

    fn desired(
        target: &str,
        fstype: &str,
        sources: &[&str],
        options: &str,
    ) -> crate::mount_converge::DesiredMount {
        crate::mount_converge::DesiredMount {
            target: target.into(),
            backend: "nfs".into(),
            fstype: fstype.into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            routes: Vec::new(),
            remount_policy: Default::default(),
            options: options.into(),
            credential: None,
        }
    }

    #[test]
    fn render_adapter_splits_primary_and_failover_and_renders_map_line() {
        // A share with willow primary + maple failover must adapt to source =
        // primary, failover_sources = the rest, and render one non-empty map
        // line (the fix for the `rendered: 0` map-wipe).
        let d = desired(
            "/mnt/data",
            "nfs4",
            &["10.10.10.10:/mnt/user/data", "10.10.10.11:/mnt/user/data"],
            "vers=4.2,soft,softreval,timeo=50,retrans=2",
        );
        let m = desired_to_render_mount(&d);
        assert_eq!(m.source, "10.10.10.10:/mnt/user/data");
        assert_eq!(
            m.failover_sources.as_deref(),
            Some("10.10.10.11:/mnt/user/data")
        );
        assert_eq!(m.kind, "network_share");
        assert_eq!(m.target, "/mnt/data");
        assert!(m.enabled);

        let body = crate::autofs::render_map(&[m]);
        let lines: Vec<&str> = body.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines.len(),
            1,
            "must render exactly one map line, not wipe to header-only"
        );
        assert!(
            lines[0].contains("10.10.10.10:/mnt/user/data"),
            "primary present"
        );
        assert!(
            lines[0].contains("10.10.10.11:/mnt/user/data"),
            "failover present"
        );
    }

    #[test]
    fn render_adapter_single_source_has_no_failover() {
        let d = desired("/mnt/solo", "nfs4", &["10.10.10.10:/mnt/user/solo"], "");
        let m = desired_to_render_mount(&d);
        assert_eq!(m.source, "10.10.10.10:/mnt/user/solo");
        assert!(m.failover_sources.is_none());
        assert!(m.options.is_none(), "empty options render as None");
    }

    fn mm(name: &str, backend: &str) -> crate::managed_mounts::ManagedMount {
        crate::managed_mounts::ManagedMount {
            name: name.into(),
            backend: backend.into(),
            kind: "network_share".into(),
            source: "server1:/export/pool".into(),
            failover_sources: None,
            target: format!("/mnt/{name}"),
            fstype: "nfs4".into(),
            options: None,
            credential: None,
            remount_policy: None,
            routes: Default::default(),
            enabled: true,
        }
    }

    #[test]
    fn plan_routes_recover_capable_backend_and_falls_back_otherwise() {
        let mounts = [mm("a", "nfs"), mm("b", "nfs"), mm("c", "smb")];
        // `nfs` is recover-capable; `smb` is not (→ autofs fallback).
        let plan = plan_recovery(
            mounts
                .iter()
                .map(|m| (m.backend.as_str(), m.target.as_str())),
            |name| name == "nfs",
        );
        assert_eq!(
            plan.backend_calls,
            vec![(
                "nfs".to_string(),
                vec!["/mnt/a".to_string(), "/mnt/b".to_string()]
            )]
        );
        assert_eq!(plan.fallback_targets, vec!["/mnt/c".to_string()]);
    }

    #[test]
    fn plan_unknown_backend_falls_back() {
        let mounts = [mm("a", "mystery")];
        let plan = plan_recovery(
            mounts
                .iter()
                .map(|m| (m.backend.as_str(), m.target.as_str())),
            |_| false,
        );
        assert!(plan.backend_calls.is_empty());
        assert_eq!(plan.fallback_targets, vec!["/mnt/a".to_string()]);
    }

    #[test]
    fn plan_all_capable_produces_no_fallback() {
        let mounts = [mm("a", "nfs"), mm("b", "smb")];
        let plan = plan_recovery(
            mounts
                .iter()
                .map(|m| (m.backend.as_str(), m.target.as_str())),
            |_| true,
        );
        assert!(plan.fallback_targets.is_empty());
        assert_eq!(plan.backend_calls.len(), 2);
        // Deterministic ordering (BTreeMap): nfs before smb.
        assert_eq!(plan.backend_calls[0].0, "nfs");
        assert_eq!(plan.backend_calls[1].0, "smb");
    }

    #[test]
    fn plan_empty_mounts_is_empty() {
        let plan = plan_recovery(std::iter::empty(), |_| true);
        assert_eq!(plan, RecoveryPlan::default());
    }

    #[test]
    fn list_args_default_deserializes_from_empty() {
        let a: StorageListArgs = serde_json::from_str("{}").unwrap();
        let _ = a; // no fields; just proves default/serde wiring
        let a2 = StorageListArgs::default();
        let _ = a2;
    }

    #[test]
    fn share_list_args_default_reads_table() {
        let a: StorageShareListArgs = serde_json::from_str("{}").unwrap();
        assert!(a.live.is_none());
        assert!(a.provider.is_none());
        // absent `live` is a table read
        assert!(!a.live.unwrap_or(false));
        let a2 = StorageShareListArgs::default();
        assert!(a2.live.is_none());
    }

    #[test]
    fn share_list_args_live_and_provider() {
        let a: StorageShareListArgs =
            serde_json::from_str(r#"{"live":true,"provider":"nfs"}"#).unwrap();
        assert_eq!(a.live, Some(true));
        assert_eq!(a.provider.as_deref(), Some("nfs"));
    }

    #[test]
    fn mount_update_args_action_optional_defaults_none() {
        let a: StorageMountUpdateArgs = serde_json::from_str("{}").unwrap();
        assert!(a.action.is_none()); // absent action = CRUD row edit
        let apply: StorageMountUpdateArgs =
            serde_json::from_str(r#"{"action":"apply","trigger":false}"#).unwrap();
        assert_eq!(apply.action, Some(StorageMountAction::Apply));
        assert_eq!(apply.trigger, Some(false));
        let recover: StorageMountUpdateArgs =
            serde_json::from_str(r#"{"action":"recover","healthTimeoutSecs":12}"#).unwrap();
        assert_eq!(recover.action, Some(StorageMountAction::Recover));
        assert_eq!(recover.health_timeout_secs, Some(12));
    }

    #[test]
    fn mount_update_args_unmount_fields() {
        let a: StorageMountUpdateArgs =
            serde_json::from_str(r#"{"action":"unmount","provider":"smb","target":"/mnt/media"}"#)
                .unwrap();
        assert_eq!(a.action, Some(StorageMountAction::Unmount));
        assert_eq!(a.provider.as_deref(), Some("smb"));
        assert_eq!(a.target.as_deref(), Some("/mnt/media"));
    }

    #[test]
    fn detail_args_view_defaults_usage() {
        let a: StorageDetailArgs =
            serde_json::from_str(r#"{"provider":"s3","id":"s3://b/p"}"#).unwrap();
        assert_eq!(a.view, StorageDetailView::Usage);
        assert_eq!(a.provider, "s3");
    }

    #[test]
    fn share_row_serializes_camel_case() {
        let row = ShareRow {
            provider: "nfs".into(),
            id: "export1".into(),
            source: "host:/export".into(),
            target: Some("/mnt/x".into()),
            fstype: "nfs4".into(),
            mounted: true,
            configured_sources: vec!["host:/export".into(), "host2:/export".into()],
        };
        let v: serde_json::Value = serde_json::to_value(&row).unwrap();
        assert_eq!(v["provider"], "nfs");
        assert_eq!(v["id"], "export1");
        assert_eq!(v["source"], "host:/export");
        assert_eq!(v["target"], "/mnt/x");
        assert_eq!(v["fstype"], "nfs4");
        assert_eq!(v["mounted"], true);
        assert_eq!(v["configuredSources"][0], "host:/export");
        assert_eq!(v["configuredSources"][1], "host2:/export");
    }

    #[test]
    fn share_row_null_target_roundtrips() {
        let row = ShareRow {
            provider: "smb".into(),
            id: "s".into(),
            source: "//nas/s".into(),
            target: None,
            fstype: "cifs".into(),
            mounted: false,
            configured_sources: vec![],
        };
        let v: serde_json::Value = serde_json::to_value(&row).unwrap();
        assert!(v["target"].is_null());
        assert!(v["configuredSources"].as_array().unwrap().is_empty());
    }

    #[test]
    fn backend_error_serializes() {
        let e = StorageBackendError {
            provider: "nfs".into(),
            error: "boom".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["provider"], "nfs");
        assert_eq!(v["error"], "boom");
    }

    #[test]
    fn shares_output_shape() {
        let out = StorageSharesOutput {
            shares: vec![],
            errors: vec![StorageBackendError {
                provider: "nfs".into(),
                error: "down".into(),
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert!(v["shares"].as_array().unwrap().is_empty());
        assert_eq!(v["errors"][0]["provider"], "nfs");
    }

    #[test]
    fn mount_output_serializes_camel_case() {
        let out = StorageMountOutput {
            rendered: 3,
            changed: vec!["/etc/auto.orca".into()],
            reloaded: true,
            triggered: vec!["/mnt/a".into()],
            userspace_mounted: vec!["/mnt/obj".into()],
            userspace_unmounted: vec![],
            errors: vec![],
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["rendered"], 3);
        assert_eq!(v["changed"][0], "/etc/auto.orca");
        assert_eq!(v["reloaded"], true);
        assert_eq!(v["triggered"][0], "/mnt/a");
        assert_eq!(v["userspaceMounted"][0], "/mnt/obj");
        assert!(v["userspaceUnmounted"].as_array().unwrap().is_empty());
        assert!(v["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn recover_output_serializes_camel_case() {
        let out = StorageRecoverOutput {
            recovered: vec!["/mnt/a".into()],
            still_stale: vec![],
            healthy: vec!["/mnt/b".into()],
            errors: vec![],
            no_stale_found: false,
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["recovered"][0], "/mnt/a");
        assert!(v["stillStale"].as_array().unwrap().is_empty());
        assert_eq!(v["healthy"][0], "/mnt/b");
        assert_eq!(v["noStaleFound"], false);
    }

    fn share_row(routes: Vec<plugin_toolkit::route::Route>) -> crate::shares::EndpointRow {
        crate::shares::EndpointRow {
            id: "share-1".into(),
            name: "data".into(),
            backend: "nfs".into(),
            fstype: "nfs4".into(),
            options: "{}".into(),
            options_rendered: "vers=4.2".into(),
            credential: None,
            routes: plugin_toolkit::route::Routes::from(routes),
            enabled: true,
        }
    }

    #[test]
    fn set_route_enabled_toggles_matching_route_only() {
        use plugin_toolkit::route::Route;
        let mut row = share_row(vec![
            Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049)),
            Route::new("lan_v4", "nfs", "10.0.0.2", Some(2049)),
        ]);
        assert!(set_route_enabled(&mut row, "10.0.0.1", false));
        let held: Vec<_> = row.routes.iter().filter(|r| !r.enabled).collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].value, "10.0.0.1");
        // A value no route carries → no change reported.
        assert!(!set_route_enabled(&mut row, "10.9.9.9", false));
    }

    #[tokio::test]
    async fn wait_source_healthy_times_out_false_for_unroutable() {
        // TEST-NET-1 never answers; a zero overall budget means one failed probe
        // then immediate timeout — no 3s sleep.
        let ok = wait_source_healthy(
            "192.0.2.1",
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(0),
        )
        .await;
        assert!(!ok);
    }

    #[test]
    fn share_update_args_action_optional_and_coord_fields() {
        let edit: StorageShareUpdateArgs =
            serde_json::from_str(r#"{"name":"data","enabled":false}"#).unwrap();
        assert!(edit.action.is_none());
        let drain: StorageShareUpdateArgs =
            serde_json::from_str(r#"{"name":"data","action":"drain","route":"10.0.0.1"}"#).unwrap();
        assert_eq!(drain.action, Some(StorageShareAction::Drain));
        assert_eq!(drain.route.as_deref(), Some("10.0.0.1"));
        let reboot: StorageShareUpdateArgs = serde_json::from_str(
            r#"{"name":"data","action":"rebootSource","route":"10.0.0.1","rebootTool":"host.reboot"}"#,
        )
        .unwrap();
        assert_eq!(reboot.action, Some(StorageShareAction::RebootSource));
        assert_eq!(reboot.reboot_tool.as_deref(), Some("host.reboot"));
    }

    #[test]
    fn list_output_serializes() {
        let out = StorageListOutput {
            providers: vec![],
            next_cursor: None,
            total: None,
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert!(v["providers"].as_array().unwrap().is_empty());
    }
}
