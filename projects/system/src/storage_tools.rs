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
    /// The share's canonical routes (failover candidates), each annotated with
    /// this placement's live state. **The route self-annotates `active`** — there
    /// is no separate `activeRoute` scalar. Derived read-only from the joined
    /// share; a placement owns no routes of its own. Empty if the share is gone.
    pub routes: Vec<MountRoute>,
    /// Whether the convergence tick observed more than one mount stacked at this
    /// target (an anomaly: the write path blocks it, a reconcile tolerates and
    /// surfaces it here). More than one `active` route implies the same.
    pub multi_mounted: bool,
    pub enabled: bool,
}

/// One of a share's canonical routes, projected onto a placement with its live
/// state. The addressing fields (`kind`/`value`/`port`/`path`/`enabled`) come
/// from `shares.routes`; `active`/`options`/`drift` are this host's tick-observed
/// reality for the route currently mounted at the target.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MountRoute {
    pub kind: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub enabled: bool,
    /// Whether THIS route is the source currently mounted at the target — the
    /// route self-annotating, replacing the old top-level `activeRoute` scalar.
    pub active: bool,
    /// Comma-joined live `-o` option tokens the kernel reports, present only on
    /// the active route (the STORED value the tick observed, so an operator sees
    /// hard-vs-soft per host without SSHing in).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    /// Whether the active route's live options diverge from the share's rendered
    /// options ("has this host drifted, still `hard`?"). Always `false` on a
    /// non-active route.
    pub drift: bool,
}

/// Project a placement row onto its API view, deriving its routes from the joined
/// `share` (the canonical route set). `share` is `None` when the placement points
/// at a share that no longer exists — the routes list is then empty.
fn mount_view(
    row: &crate::mounts::EndpointRow,
    share: Option<&crate::shares::EndpointRow>,
) -> MountView {
    let routes = share.map(|s| mount_routes(row, s)).unwrap_or_default();
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
        routes,
        multi_mounted: row.multi_mounted,
        enabled: row.enabled,
    }
}

/// Derive the annotated route set: each canonical `share.routes` entry with the
/// placement's live state. A route is `active` when its rendered source matches
/// the source the tick last mounted at the target (`row.active_route`); the live
/// `options`/`drift` the tick observed attach to that active route only.
fn mount_routes(
    row: &crate::mounts::EndpointRow,
    share: &crate::shares::EndpointRow,
) -> Vec<MountRoute> {
    let active_src = row.active_route.as_deref();
    share
        .routes
        .iter()
        .map(|r| {
            let source = crate::mount_converge::source_of_route(&share.fstype, r);
            let active = active_src == Some(source.as_str());
            MountRoute {
                kind: r.kind.clone(),
                value: r.value.clone(),
                port: r.port,
                path: r.path.clone(),
                enabled: r.enabled,
                active,
                options: if active {
                    row.active_options.clone()
                } else {
                    None
                },
                drift: active && row.drift,
            }
        })
        .collect()
}

/// Every share, indexed by its uuidv7 `id`, for joining placements to their
/// canonical routes in the read path (no fan-out; a single local table read).
fn shares_by_id() -> std::collections::HashMap<String, crate::shares::EndpointRow> {
    crate::shares::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect()
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
            replication: row.replication.clone(),
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
        replication: row.replication.clone(),
        routes: row.routes.clone(),
        enabled: row.enabled,
    }
}

// ── storage.replication.detail (config + observed status) ────────────────
// Hand-written (macro `detail` skipped) so the read folds in the relationship's
// OBSERVED health alongside its config — the read side of the config/health
// split ([[on-demand-not-poll-and-cache]]). The status is resolved host-local,
// on-demand, via the registered provider seam; with none registered (core today)
// it is `None` (unknown), which is exactly what the converge failover gate sees.

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageReplicationDetailArgs {
    /// The relationship's role label (the `replications` PK).
    #[arg(long)]
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageReplicationDetailOutput {
    /// The relationship config row — provider/folder + member routes.
    pub relationship: crate::replication::EndpointEntry,
    /// Observed sync health, resolved on read. `None` = unknown (no provider
    /// registered, or the relationship's provider has no adapter loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<storage::ReplicationStatus>,
}

/// Detail for one replication relationship, folding in its observed
/// [`ReplicationStatus`](storage::ReplicationStatus) resolved host-local on read.
#[orca_tool(domain = "storage.replication", verb = "detail")]
async fn storage_replication_detail(
    args: StorageReplicationDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageReplicationDetailOutput> {
    let row = crate::replication::endpoint_db::get(&args.name)?.ok_or_else(|| {
        plugin_toolkit::runtime::missing_row_error("storage.replication", &args.name)
    })?;
    // Members are the relationship's route values — the generic reused as
    // membership. Observe health via the registered provider (Unknown → None).
    let members: Vec<String> = row.routes.iter().map(|r| r.value.clone()).collect();
    let status = storage::resolve_replication_status(&row.provider, &row.folder, &members).await;
    Ok(StorageReplicationDetailOutput {
        relationship: crate::replication::EndpointEntry {
            name: row.name.clone(),
            id: row.id.clone(),
            provider: row.provider.clone(),
            folder: row.folder.clone(),
            routes: row.routes.clone(),
            enabled: row.enabled,
        },
        status,
    })
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
    /// Reference (uuidv7) to a replication relationship whose observed health
    /// gates this share's failover. Pass an empty string to clear it.
    #[arg(long)]
    pub replication: Option<String>,
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
    // The first pass is deliberately best-effort — non-fatal errors here are
    // expected when I/O is still in flight and the forced retry below is the
    // real attempt — so they are logged only at debug. `run_privileged`
    // collects errors into the result rather than returning a `Result`.
    let lazy = crate::autofs::run_privileged(&crate::autofs::PrivilegedOp::Unmount {
        targets: targets.clone(),
    })
    .await;
    if !lazy.errors.is_empty() {
        tracing::debug!(
            "[storage.drain] lazy unmount pass errors (will force-retry): {:?}",
            lazy.errors
        );
    }
    tokio::time::sleep(std::time::Duration::from_secs(settle_secs as u64)).await;
    // Final forced pass: if even this reports errors the placement may still be
    // mounted and the drain did not fully release it — surface that to the
    // operator.
    let forced = crate::autofs::run_privileged(&crate::autofs::PrivilegedOp::Unmount {
        targets: targets.clone(),
    })
    .await;
    if !forced.errors.is_empty() {
        tracing::warn!(
            "[storage.drain] forced unmount of {targets:?} reported errors: {:?}",
            forced.errors
        );
    }
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
    if let Some(v) = args.replication.clone() {
        // Empty string clears the ref; any other value sets it.
        row.replication = if v.is_empty() { None } else { Some(v) };
        applied.push("replication".to_string());
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
    // Multi-mount guard on a moved placement: if this edit changed host/target,
    // reject a collision with a DIFFERENT enabled placement at the same target.
    if (applied.iter().any(|a| a == "host" || a == "target"))
        && let Some(other) = mount_at_target(&row.host, &row.target)?
        && other.id != row.id
    {
        anyhow::bail!(
            "host `{}` already mounts `{}` at `{}`; two mounts at one target is \
             blocked",
            row.host,
            other.name,
            row.target
        );
    }
    let changed = crate::mounts::endpoint_db::update(&row)?;
    if !changed {
        anyhow::bail!("update reported no row change for `{}`", row.id);
    }
    let share = crate::shares::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .find(|s| s.id == row.share_id);
    Ok(StorageMountEditOutput {
        mount: mount_view(&row, share.as_ref()),
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
    let shares = shares_by_id();
    Ok(rows
        .iter()
        .filter(|m| args.host.as_deref().is_none_or(|h| m.host == h))
        .map(|m| mount_view(m, shares.get(&m.share_id)))
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
    let share = crate::shares::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .find(|s| s.id == row.share_id);
    Ok(mount_view(&row, share.as_ref()))
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
    /// Override the multi-mount guard: allow authoring a second placement whose
    /// `(host, target)` collides with an existing one. Off by default — stacking
    /// two mounts at one target is an anomaly the write path blocks.
    #[arg(long)]
    #[serde(default)]
    pub force: bool,
}

/// Author a new mount placement. A placement owns no routes — it references a
/// share, whose canonical route set is the failover truth. Errors if the host
/// already has a placement with the same `name` (`UNIQUE(host, name)`), or — the
/// multi-mount guard — a placement already targeting the same `(host, target)`,
/// unless `--force`.
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
    if !args.force
        && let Some(existing) = mount_at_target(&args.host, &args.target)?
    {
        anyhow::bail!(
            "host `{}` already mounts `{}` at `{}`; stacking two mounts at one \
             target is blocked — pass --force to override",
            args.host,
            existing.name,
            args.target
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
        multi_mounted: false,
        enabled: true,
    };
    crate::mounts::endpoint_db::insert(&row)?;
    let share = crate::shares::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .find(|s| s.id == row.share_id);
    Ok(mount_view(&row, share.as_ref()))
}

/// The existing enabled placement targeting `(host, target)`, if any — the
/// multi-mount guard's lookup. Distinct from `get_by_host_name`: this keys on the
/// mountpoint, catching a second placement (different `name`) that would stack on
/// the same target.
fn mount_at_target(host: &str, target: &str) -> anyhow::Result<Option<crate::mounts::EndpointRow>> {
    Ok(crate::mounts::endpoint_db::list()?
        .into_iter()
        .find(|m| m.enabled && m.host == host && m.target == target))
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
            replication: None,
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
            replication: None,
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

    fn mount_row(
        active_route: Option<&str>,
        active_options: Option<&str>,
        drift: bool,
        multi_mounted: bool,
    ) -> crate::mounts::EndpointRow {
        crate::mounts::EndpointRow {
            id: "m1".into(),
            name: "data".into(),
            share_id: "share-1".into(),
            host: "h1".into(),
            target: "/mnt/data".into(),
            remount_policy: None,
            health: plugin_toolkit::storage::Health::Ok,
            active_route: active_route.map(str::to_string),
            active_options: active_options.map(str::to_string),
            drift,
            multi_mounted,
            enabled: true,
        }
    }

    #[test]
    fn mount_route_self_annotates_active_with_options_and_drift() {
        use plugin_toolkit::route::Route;
        // Two candidate routes; the placement is mounted from the second, with
        // drifted options. Only that route is active and carries options/drift.
        let share = share_row(vec![
            Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049)),
            Route::new("lan_v4", "nfs", "10.0.0.2", Some(2049)),
        ]);
        let row = mount_row(Some("10.0.0.2"), Some("soft,timeo=50"), true, false);
        let routes = mount_routes(&row, &share);
        assert_eq!(routes.len(), 2);
        // Non-active route: no self-annotation.
        assert_eq!(routes[0].value, "10.0.0.1");
        assert!(!routes[0].active);
        assert!(routes[0].options.is_none());
        assert!(!routes[0].drift);
        // Active route self-annotates with the live options + drift.
        assert_eq!(routes[1].value, "10.0.0.2");
        assert!(routes[1].active);
        assert_eq!(routes[1].options.as_deref(), Some("soft,timeo=50"));
        assert!(routes[1].drift);
    }

    #[test]
    fn mount_route_none_active_when_unmounted() {
        use plugin_toolkit::route::Route;
        let share = share_row(vec![Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))]);
        let row = mount_row(None, None, false, false);
        let routes = mount_routes(&row, &share);
        assert!(
            routes
                .iter()
                .all(|r| !r.active && r.options.is_none() && !r.drift)
        );
    }

    #[test]
    fn mount_view_without_share_has_empty_routes() {
        let row = mount_row(Some("10.0.0.1"), Some("soft"), false, false);
        let view = mount_view(&row, None);
        assert!(view.routes.is_empty());
    }

    #[test]
    fn mount_view_surfaces_multi_mounted_camel_case() {
        use plugin_toolkit::route::Route;
        let share = share_row(vec![Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))]);
        let row = mount_row(Some("10.0.0.1"), Some("soft"), false, true);
        let view = mount_view(&row, Some(&share));
        assert!(view.multi_mounted);
        let v: serde_json::Value = serde_json::to_value(&view).unwrap();
        assert_eq!(v["multiMounted"], true);
        // No top-level activeRoute scalar — the route self-annotates instead.
        assert!(v.get("activeRoute").is_none());
        assert_eq!(v["routes"][0]["active"], true);
        assert_eq!(v["routes"][0]["options"], "soft");
    }

    // ── parse_remount_policy_arg ──────────────────────────────────────────
    // Serialized-string assertions (no serde_json::Value) per the coverage pass.

    #[test]
    fn parse_remount_policy_empty_and_blank_clear_to_none() {
        assert!(parse_remount_policy_arg("").unwrap().is_none());
        assert!(parse_remount_policy_arg("   ").unwrap().is_none());
        assert!(parse_remount_policy_arg("\t\n ").unwrap().is_none());
    }

    #[test]
    fn parse_remount_policy_valid_default_object() {
        let p = parse_remount_policy_arg("{}").unwrap();
        assert!(p.is_some());
        // Round-trips to the default policy.
        assert_eq!(
            p.unwrap(),
            plugin_toolkit::storage::RemountPolicy::default()
        );
    }

    #[test]
    fn parse_remount_policy_valid_with_fields() {
        let p = parse_remount_policy_arg(r#"{"aggression":"force"}"#)
            .unwrap()
            .expect("some policy");
        assert_eq!(
            p.aggression,
            plugin_toolkit::storage::RemountAggression::Force
        );
    }

    #[test]
    fn parse_remount_policy_malformed_is_hard_error() {
        let err = parse_remount_policy_arg("{not json").unwrap_err();
        assert!(
            err.to_string().contains("invalid remount policy JSON"),
            "error should name the failing arg: {err}"
        );
    }

    #[test]
    fn parse_remount_policy_wrong_type_errors() {
        // A syntactically valid JSON that is not a policy object.
        assert!(parse_remount_policy_arg("42").is_err());
    }

    // ── desired_to_render_mount: kind derivation + edge sources ───────────

    #[test]
    fn render_adapter_cifs_and_smb_are_network_share() {
        let cifs = desired_to_render_mount(&desired("/mnt/c", "cifs", &["//nas/c"], ""));
        assert_eq!(cifs.kind, "network_share");
        let smb = desired_to_render_mount(&desired("/mnt/s", "smb3", &["//nas/s"], ""));
        assert_eq!(smb.kind, "network_share");
    }

    #[test]
    fn render_adapter_non_network_fstype_is_object() {
        let obj = desired_to_render_mount(&desired("/mnt/o", "s3", &["s3://bucket/x"], ""));
        assert_eq!(obj.kind, "object");
    }

    #[test]
    fn render_adapter_empty_sources_yields_empty_source_and_no_failover() {
        let d = desired("/mnt/none", "nfs4", &[], "");
        let m = desired_to_render_mount(&d);
        assert_eq!(m.source, "");
        assert!(m.failover_sources.is_none());
        assert!(m.enabled);
    }

    #[test]
    fn render_adapter_options_preserved_when_present() {
        let m = desired_to_render_mount(&desired("/mnt/x", "nfs4", &["h:/e"], "soft,timeo=50"));
        assert_eq!(m.options.as_deref(), Some("soft,timeo=50"));
    }

    // ── arg serde: remaining Args structs ─────────────────────────────────

    #[test]
    fn exports_args_default_and_provider() {
        let a: StorageExportsArgs = serde_json::from_str("{}").unwrap();
        assert!(a.provider.is_none());
        let a2: StorageExportsArgs = serde_json::from_str(r#"{"provider":"nfs"}"#).unwrap();
        assert_eq!(a2.provider.as_deref(), Some("nfs"));
    }

    #[test]
    fn mount_list_args_host_scope() {
        let a: StorageMountListArgs = serde_json::from_str("{}").unwrap();
        assert!(a.host.is_none());
        let a2: StorageMountListArgs = serde_json::from_str(r#"{"host":"willow"}"#).unwrap();
        assert_eq!(a2.host.as_deref(), Some("willow"));
    }

    #[test]
    fn mount_detail_args_by_id_or_host_name() {
        let by_id: StorageMountDetailArgs = serde_json::from_str(r#"{"id":"m1"}"#).unwrap();
        assert_eq!(by_id.id.as_deref(), Some("m1"));
        let by_pair: StorageMountDetailArgs =
            serde_json::from_str(r#"{"host":"h1","name":"data"}"#).unwrap();
        assert_eq!(by_pair.host.as_deref(), Some("h1"));
        assert_eq!(by_pair.name.as_deref(), Some("data"));
    }

    #[test]
    fn mount_create_args_force_defaults_false() {
        let a: StorageMountCreateArgs =
            serde_json::from_str(r#"{"name":"n","shareId":"s","host":"h","target":"/mnt/t"}"#)
                .unwrap();
        assert!(!a.force);
        assert_eq!(a.share_id, "s");
        let forced: StorageMountCreateArgs = serde_json::from_str(
            r#"{"name":"n","shareId":"s","host":"h","target":"/mnt/t","force":true}"#,
        )
        .unwrap();
        assert!(forced.force);
    }

    #[test]
    fn replication_detail_args_name() {
        let a: StorageReplicationDetailArgs = serde_json::from_str(r#"{"name":"repl"}"#).unwrap();
        assert_eq!(a.name, "repl");
    }

    #[test]
    fn mount_delete_args_id() {
        let a: StorageMountDeleteArgs = serde_json::from_str(r#"{"id":"m9"}"#).unwrap();
        assert_eq!(a.id, "m9");
    }

    #[test]
    fn share_update_args_crud_fields_and_routes_default_empty() {
        let a: StorageShareUpdateArgs = serde_json::from_str(
            r#"{"name":"data","backend":"nfs","fstype":"nfs4","enabled":true}"#,
        )
        .unwrap();
        assert_eq!(a.backend.as_deref(), Some("nfs"));
        assert_eq!(a.fstype.as_deref(), Some("nfs4"));
        assert_eq!(a.enabled, Some(true));
        assert!(a.routes.is_empty(), "routes default to empty");
    }

    // ── value-enum serde round-trips (camelCase) ──────────────────────────

    #[test]
    fn share_action_serde_camel_case() {
        assert_eq!(
            serde_json::to_string(&StorageShareAction::RebootSource).unwrap(),
            r#""rebootSource""#
        );
        let d: StorageShareAction = serde_json::from_str(r#""drain""#).unwrap();
        assert_eq!(d, StorageShareAction::Drain);
        let r: StorageShareAction = serde_json::from_str(r#""resume""#).unwrap();
        assert_eq!(r, StorageShareAction::Resume);
    }

    #[test]
    fn mount_action_serde_camel_case() {
        assert_eq!(
            serde_json::to_string(&StorageMountAction::Apply).unwrap(),
            r#""apply""#
        );
        let u: StorageMountAction = serde_json::from_str(r#""unmount""#).unwrap();
        assert_eq!(u, StorageMountAction::Unmount);
        let r: StorageMountAction = serde_json::from_str(r#""recover""#).unwrap();
        assert_eq!(r, StorageMountAction::Recover);
    }

    #[test]
    fn detail_view_serde_usage() {
        assert_eq!(
            serde_json::to_string(&StorageDetailView::Usage).unwrap(),
            r#""usage""#
        );
        assert_eq!(StorageDetailView::default(), StorageDetailView::Usage);
    }

    // ── share_entry projection (credential → has_credential) ──────────────

    #[test]
    fn share_entry_folds_credential_presence() {
        let mut row = share_row(vec![]);
        row.credential = Some("secret-ref".into());
        let e = share_entry(&row);
        assert!(e.has_credential);
        assert_eq!(e.name, "data");
        assert_eq!(e.backend, "nfs");

        row.credential = None;
        let e2 = share_entry(&row);
        assert!(!e2.has_credential);
    }

    // ── output shaping (serialized strings, no Value) ─────────────────────

    #[test]
    fn export_row_serializes_camel_case() {
        let row = ExportRow {
            provider: "nfs".into(),
            path: "/export/data".into(),
            allowed_clients: vec!["10.0.0.0/24".into()],
            options: vec!["rw".into(), "sync".into()],
            fsid: Some("0".into()),
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains(r#""provider":"nfs""#));
        assert!(s.contains(r#""path":"/export/data""#));
        assert!(s.contains(r#""allowedClients":["10.0.0.0/24"]"#));
        assert!(s.contains(r#""fsid":"0""#));
    }

    #[test]
    fn export_row_null_fsid_serializes() {
        let row = ExportRow {
            provider: "nfs".into(),
            path: "/e".into(),
            allowed_clients: vec![],
            options: vec![],
            fsid: None,
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains(r#""fsid":null"#));
    }

    #[test]
    fn exports_output_shape() {
        let out = StorageExportsOutput {
            exports: vec![],
            errors: vec![StorageBackendError {
                provider: "nfs".into(),
                error: "unreachable".into(),
            }],
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""exports":[]"#));
        assert!(s.contains(r#""error":"unreachable""#));
    }

    #[test]
    fn mount_delete_output_serializes() {
        let out = StorageMountDeleteOutput {
            id: "m1".into(),
            changed: true,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""id":"m1""#));
        assert!(s.contains(r#""changed":true"#));
    }

    #[test]
    fn share_coord_output_omits_none_source_healthy() {
        let out = StorageShareCoordOutput {
            share: share_entry(&share_row(vec![])),
            route: "10.0.0.1".into(),
            held: true,
            source_healthy: None,
            steps: vec!["held route 10.0.0.1".into()],
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""held":true"#));
        assert!(s.contains(r#""route":"10.0.0.1""#));
        assert!(
            !s.contains("sourceHealthy"),
            "None source_healthy is skipped"
        );

        let out2 = StorageShareCoordOutput {
            source_healthy: Some(true),
            ..out
        };
        let s2 = serde_json::to_string(&out2).unwrap();
        assert!(s2.contains(r#""sourceHealthy":true"#));
    }

    // ── untagged enum shapes ──────────────────────────────────────────────

    #[test]
    fn share_update_output_untagged_variants_distinguishable() {
        let edit = StorageShareUpdateOutput::Edit(StorageShareEditOutput {
            share: share_entry(&share_row(vec![])),
            applied: vec!["enabled".into()],
        });
        let se = serde_json::to_string(&edit).unwrap();
        assert!(se.contains(r#""applied":["enabled"]"#));

        let coord = StorageShareUpdateOutput::Coord(StorageShareCoordOutput {
            share: share_entry(&share_row(vec![])),
            route: "r".into(),
            held: false,
            source_healthy: None,
            steps: vec![],
        });
        let sc = serde_json::to_string(&coord).unwrap();
        assert!(sc.contains(r#""held":false"#));
        assert!(sc.contains(r#""route":"r""#));
        // The coord shape carries no `applied` key (distinguishes it from Edit).
        assert!(!sc.contains("applied"));
    }

    #[test]
    fn mount_update_output_untagged_apply_variant() {
        let apply = StorageMountUpdateOutput::Apply(StorageMountOutput {
            rendered: 1,
            changed: vec![],
            reloaded: false,
            triggered: vec![],
            userspace_mounted: vec![],
            userspace_unmounted: vec![],
            errors: vec![],
        });
        let s = serde_json::to_string(&apply).unwrap();
        assert!(s.contains(r#""rendered":1"#));
    }

    #[test]
    fn share_list_output_untagged_registered_vs_live() {
        let reg = StorageShareListOutput::Registered(StorageShareRegisteredList {
            shares: vec![],
            next_cursor: Some("c".into()),
            total: Some(0),
        });
        let sr = serde_json::to_string(&reg).unwrap();
        assert!(sr.contains(r#""nextCursor":"c""#));

        let live = StorageShareListOutput::Live(StorageSharesOutput {
            shares: vec![],
            errors: vec![],
        });
        let sl = serde_json::to_string(&live).unwrap();
        // Live shape carries an `errors` key; Registered does not.
        assert!(sl.contains(r#""errors":[]"#));
    }

    // ── MergedRecover default ─────────────────────────────────────────────

    #[test]
    fn merged_recover_default_is_empty() {
        let m = MergedRecover::default();
        assert!(m.recovered.is_empty());
        assert!(m.still_stale.is_empty());
        assert!(m.healthy.is_empty());
        assert!(m.remounted.is_empty());
        assert!(m.still_missing.is_empty());
        assert!(m.errors.is_empty());
        assert!(!m.no_stale_found);
    }

    // ── plan_recovery: grouping details ───────────────────────────────────

    #[test]
    fn plan_recovery_preserves_target_order_and_groups_same_backend() {
        // Same backend appearing across interleaved entries collapses to one call
        // with targets in encounter order.
        let entries = [
            ("nfs", "/mnt/a"),
            ("smb", "/mnt/x"),
            ("nfs", "/mnt/b"),
            ("nfs", "/mnt/c"),
        ];
        let plan = plan_recovery(entries.iter().map(|(b, t)| (*b, *t)), |_| true);
        // BTreeMap ⇒ nfs before smb.
        assert_eq!(plan.backend_calls[0].0, "nfs");
        assert_eq!(
            plan.backend_calls[0].1,
            vec![
                "/mnt/a".to_string(),
                "/mnt/b".to_string(),
                "/mnt/c".to_string()
            ]
        );
        assert_eq!(plan.backend_calls[1].0, "smb");
        assert_eq!(plan.backend_calls[1].1, vec!["/mnt/x".to_string()]);
    }

    // ── mount_view full field mapping ─────────────────────────────────────

    #[test]
    fn mount_view_maps_refs_and_scalar_fields() {
        use plugin_toolkit::route::Route;
        let share = share_row(vec![Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))]);
        let row = mount_row(Some("10.0.0.1"), Some("soft"), false, false);
        let view = mount_view(&row, Some(&share));
        assert_eq!(view.id, "m1");
        assert_eq!(view.name, "data");
        assert_eq!(view.share.id, "share-1");
        assert_eq!(view.host.id, "h1");
        assert_eq!(view.target, "/mnt/data");
        assert!(view.enabled);
        assert!(!view.multi_mounted);
        assert_eq!(view.routes.len(), 1);
    }

    // ── mount_routes: active matching through source_of_route (path branches) ──

    #[test]
    fn mount_routes_active_matches_nfs_source_with_path() {
        use plugin_toolkit::route::Route;
        // An nfs route carrying a path renders its source as `value:path`; the
        // placement's active_route must equal that rendered source to be active.
        let mut r = Route::new("lan_v4", "nfs", "10.0.0.5", Some(2049));
        r.path = Some("/export/data".into());
        let share = share_row(vec![r]);
        let row = mount_row(Some("10.0.0.5:/export/data"), Some("hard"), false, false);
        let routes = mount_routes(&row, &share);
        assert_eq!(routes.len(), 1);
        assert!(routes[0].active, "rendered nfs source matched active_route");
        assert_eq!(routes[0].path.as_deref(), Some("/export/data"));
        assert_eq!(routes[0].options.as_deref(), Some("hard"));
    }

    #[test]
    fn mount_routes_active_matches_cifs_source_with_path() {
        use plugin_toolkit::route::Route;
        // A non-nfs (cifs) route with a path renders `//value/path`.
        let mut r = Route::new("lan_v4", "smb", "nas", None);
        r.path = Some("/media".into());
        let mut share = share_row(vec![r]);
        share.fstype = "cifs".into();
        let row = mount_row(Some("//nas/media"), None, false, false);
        let routes = mount_routes(&row, &share);
        assert!(
            routes[0].active,
            "rendered cifs source matched active_route"
        );
        // active but no live options observed → options stays None.
        assert!(routes[0].options.is_none());
    }

    #[test]
    fn mount_routes_active_route_mismatch_leaves_all_inactive() {
        use plugin_toolkit::route::Route;
        // active_route names a source none of the rendered routes produce.
        let share = share_row(vec![Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049))]);
        let row = mount_row(Some("10.9.9.9:/nope"), Some("soft"), true, false);
        let routes = mount_routes(&row, &share);
        assert!(
            routes
                .iter()
                .all(|r| !r.active && r.options.is_none() && !r.drift)
        );
    }

    // ── MountRoute serde: skip_serializing_if on port/path/options ─────────

    #[test]
    fn mount_route_omits_none_port_path_options() {
        let r = MountRoute {
            kind: "lan_v4".into(),
            value: "10.0.0.1".into(),
            port: None,
            path: None,
            enabled: true,
            active: false,
            options: None,
            drift: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("port"));
        assert!(!s.contains("path"));
        assert!(!s.contains("options"));
        assert!(s.contains(r#""kind":"lan_v4""#));
        assert!(s.contains(r#""active":false"#));
    }

    #[test]
    fn mount_route_emits_present_port_path_options() {
        let r = MountRoute {
            kind: "lan_v4".into(),
            value: "10.0.0.1".into(),
            port: Some(2049),
            path: Some("/export".into()),
            enabled: true,
            active: true,
            options: Some("hard".into()),
            drift: true,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""port":2049"#));
        assert!(s.contains(r#""path":"/export""#));
        assert!(s.contains(r#""options":"hard""#));
        assert!(s.contains(r#""drift":true"#));
    }

    // ── MountRef serde ─────────────────────────────────────────────────────

    #[test]
    fn mount_ref_serializes_bare_id() {
        let r = MountRef {
            id: "share-1".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"id":"share-1"}"#);
    }

    // ── MountView: remount_policy skip-if-none vs present ───────────────────

    #[test]
    fn mount_view_omits_none_remount_policy() {
        let row = mount_row(None, None, false, false);
        let view = mount_view(&row, None);
        assert!(view.remount_policy.is_none());
        let s = serde_json::to_string(&view).unwrap();
        assert!(!s.contains("remountPolicy"));
        // health is a stored scalar, always present.
        assert!(s.contains("health"));
    }

    #[test]
    fn mount_view_emits_present_remount_policy() {
        let mut row = mount_row(None, None, false, false);
        row.remount_policy = Some(plugin_toolkit::storage::RemountPolicy::default());
        let view = mount_view(&row, None);
        assert!(view.remount_policy.is_some());
        let s = serde_json::to_string(&view).unwrap();
        assert!(s.contains("remountPolicy"));
    }

    // ── StorageMountEditOutput serde shape ─────────────────────────────────

    #[test]
    fn mount_edit_output_serializes() {
        let row = mount_row(None, None, false, false);
        let out = StorageMountEditOutput {
            mount: mount_view(&row, None),
            applied: vec!["target".into(), "enabled".into()],
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""applied":["target","enabled"]"#));
        assert!(s.contains(r#""mount""#));
        assert!(s.contains(r#""id":"m1""#));
    }

    // ── StorageMountUpdateOutput untagged Recover variant ──────────────────

    #[test]
    fn mount_update_output_untagged_recover_variant() {
        let out = StorageMountUpdateOutput::Recover(StorageRecoverOutput {
            recovered: vec!["/mnt/a".into()],
            still_stale: vec![],
            healthy: vec![],
            errors: vec![],
            no_stale_found: false,
        });
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""recovered":["/mnt/a"]"#));
        assert!(s.contains(r#""noStaleFound":false"#));
    }

    // ── arg serde: limit/cursor paging fields ──────────────────────────────

    #[test]
    fn list_args_limit_and_cursor() {
        let a: StorageListArgs = serde_json::from_str(r#"{"limit":25,"cursor":"abc"}"#).unwrap();
        assert_eq!(a.limit, Some(25));
        assert_eq!(a.cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn share_list_args_limit_and_cursor() {
        let a: StorageShareListArgs =
            serde_json::from_str(r#"{"limit":10,"cursor":"page2"}"#).unwrap();
        assert_eq!(a.limit, Some(10));
        assert_eq!(a.cursor.as_deref(), Some("page2"));
        // table-read args coexist with (absent) live discovery flag.
        assert!(a.live.is_none());
    }

    #[test]
    fn mount_update_args_crud_placement_fields() {
        let a: StorageMountUpdateArgs = serde_json::from_str(
            r#"{"id":"m1","name":"data","shareId":"s1","host":"willow","target":"/mnt/d","enabled":false}"#,
        )
        .unwrap();
        assert!(a.action.is_none());
        assert_eq!(a.id.as_deref(), Some("m1"));
        assert_eq!(a.share_id.as_deref(), Some("s1"));
        assert_eq!(a.host.as_deref(), Some("willow"));
        assert_eq!(a.target.as_deref(), Some("/mnt/d"));
        assert_eq!(a.enabled, Some(false));
    }

    #[test]
    fn mount_create_args_remount_policy_optional() {
        let a: StorageMountCreateArgs = serde_json::from_str(
            r#"{"name":"n","shareId":"s","host":"h","target":"/mnt/t","remountPolicy":"{}"}"#,
        )
        .unwrap();
        assert_eq!(a.remount_policy.as_deref(), Some("{}"));
    }

    // ── desired_to_render_mount: backend/credential propagation + 3 sources ─

    #[test]
    fn render_adapter_propagates_backend_and_credential() {
        let d = crate::mount_converge::DesiredMount {
            target: "/mnt/sec".into(),
            backend: "smb".into(),
            fstype: "cifs".into(),
            sources: vec!["//nas/sec".into()],
            routes: Vec::new(),
            remount_policy: Default::default(),
            replication: None,
            options: "vers=3.0".into(),
            credential: Some("cred-ref".into()),
        };
        let m = desired_to_render_mount(&d);
        assert_eq!(m.backend, "smb");
        assert_eq!(m.credential.as_deref(), Some("cred-ref"));
        assert_eq!(m.name, "/mnt/sec", "name mirrors target");
        // remount_policy is a plan concern, never a render-input.
        assert!(m.remount_policy.is_none());
    }

    #[test]
    fn render_adapter_three_sources_newline_joins_failover() {
        let d = desired("/mnt/t", "nfs4", &["a:/e", "b:/e", "c:/e"], "");
        let m = desired_to_render_mount(&d);
        assert_eq!(m.source, "a:/e");
        assert_eq!(m.failover_sources.as_deref(), Some("b:/e\nc:/e"));
    }

    #[test]
    fn render_adapter_nfs3_is_network_share() {
        let m = desired_to_render_mount(&desired("/mnt/n3", "nfs3", &["h:/e"], ""));
        assert_eq!(m.kind, "network_share");
    }

    // ── StorageShareRegisteredList: skip-if-none cursor/total ───────────────

    #[test]
    fn share_registered_list_omits_none_cursor_and_total() {
        let out = StorageShareRegisteredList {
            shares: vec![],
            next_cursor: None,
            total: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("nextCursor"));
        assert!(!s.contains("total"));
        assert!(s.contains(r#""shares":[]"#));
    }

    #[test]
    fn list_output_omits_none_cursor_and_total() {
        let out = StorageListOutput {
            providers: vec![],
            next_cursor: None,
            total: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("nextCursor"));
        assert!(!s.contains("total"));
    }

    #[test]
    fn list_output_emits_present_cursor_and_total() {
        let out = StorageListOutput {
            providers: vec![],
            next_cursor: Some("nxt".into()),
            total: Some(7),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""nextCursor":"nxt""#));
        assert!(s.contains(r#""total":7"#));
    }

    // ── StorageReplicationDetailOutput: skip-if-none status ─────────────────

    #[test]
    fn replication_detail_output_omits_none_status() {
        let out = StorageReplicationDetailOutput {
            relationship: crate::replication::EndpointEntry {
                name: "repl".into(),
                id: "r1".into(),
                provider: "zfs".into(),
                folder: "tank/data".into(),
                routes: Default::default(),
                enabled: true,
            },
            status: None,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("status"));
        assert!(s.contains(r#""name":"repl""#));
        assert!(s.contains(r#""folder":"tank/data""#));
    }

    // ── StorageShareEditOutput serde shape ─────────────────────────────────

    #[test]
    fn share_edit_output_serializes() {
        let out = StorageShareEditOutput {
            share: share_entry(&share_row(vec![])),
            applied: vec!["backend".into(), "routes".into()],
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""applied":["backend","routes"]"#));
        assert!(s.contains(r#""share""#));
    }

    // ── StorageShareCoordOutput steps trail ────────────────────────────────

    #[test]
    fn share_coord_output_carries_steps_trail() {
        let out = StorageShareCoordOutput {
            share: share_entry(&share_row(vec![])),
            route: "10.0.0.1".into(),
            held: false,
            source_healthy: Some(false),
            steps: vec!["drained".into(), "rebooted".into(), "resumed".into()],
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains(r#""steps":["drained","rebooted","resumed"]"#));
        assert!(s.contains(r#""sourceHealthy":false"#));
    }

    // ── MergedRecover Clone + populated no_stale_found derivation ───────────

    #[test]
    fn merged_recover_clone_is_independent() {
        let mut m = MergedRecover::default();
        m.recovered.push("/mnt/a".into());
        m.no_stale_found = true;
        let c = m.clone();
        m.recovered.push("/mnt/b".into());
        assert_eq!(c.recovered, vec!["/mnt/a".to_string()]);
        assert!(c.no_stale_found);
    }

    // ── set_route_enabled: enable path + multi-value match ─────────────────

    #[test]
    fn set_route_enabled_enables_and_matches_all_with_value() {
        use plugin_toolkit::route::Route;
        let mut row = share_row(vec![
            Route::new("lan_v4", "nfs", "10.0.0.1", Some(2049)),
            Route::new("lan_v6", "nfs", "10.0.0.1", Some(2049)),
        ]);
        // Two routes share the same value → both toggled, one `found=true`.
        assert!(row.routes.iter().all(|r| r.enabled));
        assert!(set_route_enabled(&mut row, "10.0.0.1", false));
        assert!(row.routes.iter().all(|r| !r.enabled));
        assert!(set_route_enabled(&mut row, "10.0.0.1", true));
        assert!(row.routes.iter().all(|r| r.enabled));
    }

    // ── backend_recover_capable: unknown backend is not recover-capable ────

    #[test]
    fn backend_recover_capable_unknown_backend_is_false() {
        // No backend is registered under this name in a bare test process, so the
        // `unwrap_or(false)` branch is exercised.
        assert!(!backend_recover_capable("definitely-not-a-real-backend"));
    }

    // ── DB-backed helpers: exercised against an isolated thread-local sqlite ─
    //
    // `with_thread_db_path` scopes a private on-disk db to this test thread, so
    // these never race the rest of the suite. `open_default` runs `apply_schema`
    // and `apply_fragments` materialises the endpoint tables (`shares`,
    // `mounts`).

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

    fn seed_share() -> crate::shares::EndpointRow {
        let row = crate::shares::EndpointRow {
            id: "sh-1".into(),
            name: "data".into(),
            backend: "nfs".into(),
            fstype: "nfs4".into(),
            options: "{}".into(),
            options_rendered: "vers=4.2".into(),
            credential: None,
            replication: None,
            routes: Default::default(),
            enabled: true,
        };
        crate::shares::endpoint_db::insert(&row).expect("insert share");
        row
    }

    fn seed_mount(id: &str, host: &str, target: &str) -> crate::mounts::EndpointRow {
        let row = crate::mounts::EndpointRow {
            id: id.into(),
            name: format!("m-{id}"),
            share_id: "sh-1".into(),
            host: host.into(),
            target: target.into(),
            remount_policy: None,
            health: plugin_toolkit::storage::Health::Ok,
            active_route: None,
            active_options: None,
            drift: false,
            multi_mounted: false,
            enabled: true,
        };
        crate::mounts::endpoint_db::insert(&row).expect("insert mount");
        row
    }

    #[test]
    fn share_row_edit_applies_scalar_fields_and_persists() {
        with_db("shares_edit.db", || {
            seed_share();
            let args: StorageShareUpdateArgs = serde_json::from_str(
                r#"{"name":"data","backend":"smb","fstype":"cifs","options":"{\"x\":1}","optionsRendered":"vers=3.0","credential":"cred-ref","enabled":false}"#,
            )
            .unwrap();
            let out = share_row_edit(&args).expect("edit ok");
            // Every changed field is reported.
            for f in [
                "backend",
                "fstype",
                "options",
                "options_rendered",
                "credential",
                "enabled",
            ] {
                assert!(out.applied.iter().any(|a| a == f), "missing applied {f}");
            }
            // Persisted round-trip.
            let stored = crate::shares::endpoint_db::get("data").unwrap().unwrap();
            assert_eq!(stored.backend, "smb");
            assert_eq!(stored.fstype, "cifs");
            assert_eq!(stored.options_rendered, "vers=3.0");
            assert_eq!(stored.credential.as_deref(), Some("cred-ref"));
            assert!(!stored.enabled);
        });
    }

    #[test]
    fn share_row_edit_empty_replication_clears_ref() {
        with_db("shares_repl.db", || {
            let mut row = seed_share();
            row.replication = Some("rel-9".into());
            crate::shares::endpoint_db::update(&row).unwrap();
            let args: StorageShareUpdateArgs =
                serde_json::from_str(r#"{"name":"data","replication":""}"#).unwrap();
            let out = share_row_edit(&args).expect("edit ok");
            assert!(out.applied.iter().any(|a| a == "replication"));
            let stored = crate::shares::endpoint_db::get("data").unwrap().unwrap();
            assert!(stored.replication.is_none(), "empty string clears the ref");
        });
    }

    #[test]
    fn share_row_edit_replaces_routes() {
        with_db("shares_routes.db", || {
            seed_share();
            let args: StorageShareUpdateArgs = serde_json::from_str(
                r#"{"name":"data","routes":[{"kind":"lan_v4","scheme":"nfs","value":"10.0.0.9","port":2049}]}"#,
            )
            .unwrap();
            let out = share_row_edit(&args).expect("edit ok");
            assert!(out.applied.iter().any(|a| a == "routes"));
            let stored = crate::shares::endpoint_db::get("data").unwrap().unwrap();
            assert_eq!(stored.routes.iter().count(), 1);
            assert_eq!(stored.routes.iter().next().unwrap().value, "10.0.0.9");
        });
    }

    #[test]
    fn share_row_edit_no_fields_is_error() {
        with_db("shares_nofields.db", || {
            seed_share();
            let args: StorageShareUpdateArgs = serde_json::from_str(r#"{"name":"data"}"#).unwrap();
            let err = share_row_edit(&args).unwrap_err();
            assert!(err.to_string().contains("no fields to update"));
        });
    }

    #[test]
    fn share_row_edit_missing_row_is_error() {
        with_db("shares_missing.db", || {
            let args: StorageShareUpdateArgs =
                serde_json::from_str(r#"{"name":"ghost","backend":"nfs"}"#).unwrap();
            let err = share_row_edit(&args).unwrap_err();
            assert!(err.to_string().to_lowercase().contains("ghost"));
        });
    }

    #[test]
    fn mount_row_edit_requires_id() {
        with_db("mount_noid.db", || {
            let args: StorageMountUpdateArgs =
                serde_json::from_str(r#"{"target":"/mnt/x"}"#).unwrap();
            let err = mount_row_edit(&args).unwrap_err();
            assert!(err.to_string().contains("`id` is required"));
        });
    }

    #[test]
    fn mount_row_edit_applies_and_persists() {
        with_db("mount_edit.db", || {
            seed_share();
            seed_mount("m-1", "h1", "/mnt/data");
            let args: StorageMountUpdateArgs = serde_json::from_str(
                r#"{"id":"m-1","name":"data2","target":"/mnt/data2","remountPolicy":"{\"aggression\":\"force\"}","enabled":false}"#,
            )
            .unwrap();
            let out = mount_row_edit(&args).expect("edit ok");
            for f in ["name", "target", "remount_policy", "enabled"] {
                assert!(out.applied.iter().any(|a| a == f), "missing applied {f}");
            }
            let stored = crate::mounts::endpoint_db::get_by_id("m-1")
                .unwrap()
                .unwrap();
            assert_eq!(stored.target, "/mnt/data2");
            assert!(!stored.enabled);
            assert_eq!(
                stored.remount_policy.unwrap().aggression,
                plugin_toolkit::storage::RemountAggression::Force
            );
        });
    }

    #[test]
    fn mount_row_edit_missing_row_is_error() {
        with_db("mount_missing.db", || {
            let args: StorageMountUpdateArgs =
                serde_json::from_str(r#"{"id":"nope","enabled":true}"#).unwrap();
            let err = mount_row_edit(&args).unwrap_err();
            assert!(err.to_string().to_lowercase().contains("nope"));
        });
    }

    #[test]
    fn mount_row_edit_blocks_collision_at_target() {
        with_db("mount_collision.db", || {
            seed_share();
            // Existing enabled placement occupies /mnt/shared on h1.
            seed_mount("m-existing", "h1", "/mnt/shared");
            // A second placement on the same host, elsewhere; move it onto the
            // occupied target and expect the multi-mount guard to fire.
            seed_mount("m-move", "h1", "/mnt/other");
            let args: StorageMountUpdateArgs =
                serde_json::from_str(r#"{"id":"m-move","target":"/mnt/shared"}"#).unwrap();
            let err = mount_row_edit(&args).unwrap_err();
            assert!(
                err.to_string().contains("two mounts at one target is"),
                "expected collision guard: {err}"
            );
        });
    }

    #[test]
    fn mount_at_target_finds_and_misses() {
        with_db("mount_at_target.db", || {
            seed_share();
            seed_mount("m-1", "h1", "/mnt/data");
            let hit = mount_at_target("h1", "/mnt/data").unwrap();
            assert_eq!(hit.unwrap().id, "m-1");
            assert!(mount_at_target("h1", "/mnt/nowhere").unwrap().is_none());
            assert!(
                mount_at_target("other-host", "/mnt/data")
                    .unwrap()
                    .is_none()
            );
        });
    }

    // ── shares_by_id (DB projection) ──────────────────────────────────────

    #[test]
    fn shares_by_id_keys_rows_by_uuid() {
        with_db("shares_by_id.db", || {
            seed_share(); // id = "sh-1"
            let map = shares_by_id();
            assert_eq!(map.len(), 1);
            assert_eq!(map.get("sh-1").unwrap().name, "data");
            assert!(!map.contains_key("missing"));
        });
    }

    #[test]
    fn shares_by_id_empty_db_is_empty_map() {
        with_db("shares_by_id_empty.db", || {
            assert!(shares_by_id().is_empty());
        });
    }

    // ── storage_mount_create / delete (tool bodies; DB only, no network) ───
    //
    // The `_ctx` argument is unused by these bodies, so a bare ToolCtx over a
    // throwaway Config suffices — all state flows through the thread-local db.

    fn test_ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-st-ctx-{}-{}", std::process::id(), n));
        contract::ToolCtx::new(std::sync::Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("storage-test.db"),
            ports: Default::default(),
        }))
    }

    /// A current-thread runtime so `block_on` runs the async tool body on THIS
    /// thread — the one `with_thread_db_path` scoped the db to. `#[tokio::test]`
    /// cannot be used: its runtime is already active, so a nested `block_on`
    /// inside the sync `with_thread_db_path` closure would panic.
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    #[test]
    fn storage_mount_create_inserts_and_returns_view() {
        with_db("mount_create_ok.db", || {
            seed_share();
            let ctx = test_ctx();
            let args: StorageMountCreateArgs = serde_json::from_str(
                r#"{"name":"data","shareId":"sh-1","host":"h1","target":"/mnt/data"}"#,
            )
            .unwrap();
            let view = rt()
                .block_on(storage_mount_create(args, &ctx))
                .expect("create ok");
            assert_eq!(view.name, "data");
            assert_eq!(view.target, "/mnt/data");
            assert_eq!(view.share.id, "sh-1");
            assert!(
                crate::mounts::endpoint_db::get_by_host_name("h1", "data")
                    .unwrap()
                    .is_some()
            );
        });
    }

    #[test]
    fn storage_mount_create_rejects_duplicate_name_on_host() {
        with_db("mount_create_dup.db", || {
            seed_share();
            let row = crate::mounts::EndpointRow {
                id: "m-existing".into(),
                name: "dup".into(),
                share_id: "sh-1".into(),
                host: "h1".into(),
                target: "/mnt/a".into(),
                remount_policy: None,
                health: plugin_toolkit::storage::Health::Ok,
                active_route: None,
                active_options: None,
                drift: false,
                multi_mounted: false,
                enabled: true,
            };
            crate::mounts::endpoint_db::insert(&row).unwrap();
            let ctx = test_ctx();
            let args: StorageMountCreateArgs = serde_json::from_str(
                r#"{"name":"dup","shareId":"sh-1","host":"h1","target":"/mnt/b"}"#,
            )
            .unwrap();
            let err = rt().block_on(storage_mount_create(args, &ctx)).unwrap_err();
            assert!(err.to_string().contains("already exists"), "{err}");
        });
    }

    #[test]
    fn storage_mount_create_blocks_target_collision_unless_forced() {
        with_db("mount_create_collision.db", || {
            seed_share();
            seed_mount("occupant", "h1", "/mnt/shared");
            let ctx = test_ctx();
            let args: StorageMountCreateArgs = serde_json::from_str(
                r#"{"name":"newname","shareId":"sh-1","host":"h1","target":"/mnt/shared"}"#,
            )
            .unwrap();
            let err = rt().block_on(storage_mount_create(args, &ctx)).unwrap_err();
            assert!(
                err.to_string().contains("stacking two mounts at one"),
                "{err}"
            );

            let forced: StorageMountCreateArgs = serde_json::from_str(
                r#"{"name":"newname","shareId":"sh-1","host":"h1","target":"/mnt/shared","force":true}"#,
            )
            .unwrap();
            let view = rt()
                .block_on(storage_mount_create(forced, &ctx))
                .expect("force create ok");
            assert_eq!(view.name, "newname");
        });
    }

    #[test]
    fn storage_mount_delete_is_idempotent() {
        with_db("mount_delete.db", || {
            seed_share();
            seed_mount("m-1", "h1", "/mnt/data");
            let ctx = test_ctx();
            let out = rt()
                .block_on(storage_mount_delete(
                    StorageMountDeleteArgs { id: "m-1".into() },
                    &ctx,
                ))
                .expect("delete ok");
            assert_eq!(out.id, "m-1");
            assert!(out.changed);
            let again = rt()
                .block_on(storage_mount_delete(
                    StorageMountDeleteArgs { id: "m-1".into() },
                    &ctx,
                ))
                .expect("delete ok");
            assert!(!again.changed);
        });
    }

    // ── storage_mount_update: unmount dispatch guard branches ─────────────
    // The action=unmount arm validates `provider`/`target` synchronously before
    // any backend call, so these error guards are reachable without network.

    #[test]
    fn storage_mount_update_unmount_requires_provider() {
        with_db("mu_unmount_noprovider.db", || {
            let ctx = test_ctx();
            let args: StorageMountUpdateArgs =
                serde_json::from_str(r#"{"action":"unmount","target":"/mnt/x"}"#).unwrap();
            let err = rt().block_on(storage_mount_update(args, &ctx)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("`provider` is required for action=unmount"),
                "{err}"
            );
        });
    }

    #[test]
    fn storage_mount_update_unmount_requires_target() {
        with_db("mu_unmount_notarget.db", || {
            let ctx = test_ctx();
            let args: StorageMountUpdateArgs =
                serde_json::from_str(r#"{"action":"unmount","provider":"nfs"}"#).unwrap();
            let err = rt().block_on(storage_mount_update(args, &ctx)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("`target` is required for action=unmount"),
                "{err}"
            );
        });
    }

    #[test]
    fn storage_mount_update_unmount_unknown_backend_errors() {
        // Both required fields present → dispatch reaches `mount_unmount`, whose
        // `backend()` lookup misses in a bare test process (no backend named
        // `ghost-backend` is registered) → the unknown-backend error branch.
        with_db("mu_unmount_unknown.db", || {
            let ctx = test_ctx();
            let args: StorageMountUpdateArgs = serde_json::from_str(
                r#"{"action":"unmount","provider":"ghost-backend","target":"/mnt/x"}"#,
            )
            .unwrap();
            let err = rt().block_on(storage_mount_update(args, &ctx)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("no storage backend named `ghost-backend`"),
                "{err}"
            );
        });
    }

    // ── storage_detail: unknown-provider sync guard ───────────────────────

    #[test]
    fn storage_detail_unknown_provider_errors() {
        with_db("detail_unknown.db", || {
            let ctx = test_ctx();
            let args = StorageDetailArgs {
                view: StorageDetailView::Usage,
                provider: "ghost-backend".into(),
                id: "s3://b/p".into(),
            };
            let err = rt().block_on(storage_detail(args, &ctx)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("no storage backend named `ghost-backend`"),
                "{err}"
            );
        });
    }

    // ── storage_mount_detail: selector guards + DB lookups ────────────────

    #[test]
    fn storage_mount_detail_requires_a_selector() {
        with_db("md_noselector.db", || {
            let ctx = test_ctx();
            let args = StorageMountDetailArgs::default();
            let err = rt().block_on(storage_mount_detail(args, &ctx)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("pass `--id`, or both `--host` and `--name`"),
                "{err}"
            );
        });
    }

    #[test]
    fn storage_mount_detail_missing_id_errors() {
        with_db("md_missing_id.db", || {
            let ctx = test_ctx();
            let args = StorageMountDetailArgs {
                id: Some("does-not-exist".into()),
                ..Default::default()
            };
            let err = rt().block_on(storage_mount_detail(args, &ctx)).unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("does-not-exist"),
                "{err}"
            );
        });
    }

    #[test]
    fn storage_mount_detail_missing_host_name_errors() {
        with_db("md_missing_hostname.db", || {
            let ctx = test_ctx();
            let args = StorageMountDetailArgs {
                host: Some("h1".into()),
                name: Some("ghost".into()),
                ..Default::default()
            };
            let err = rt().block_on(storage_mount_detail(args, &ctx)).unwrap_err();
            assert!(
                err.to_string()
                    .contains("no mount placement `ghost` on host `h1`"),
                "{err}"
            );
        });
    }

    #[test]
    fn storage_mount_detail_by_id_returns_view() {
        with_db("md_by_id.db", || {
            seed_share();
            seed_mount("m-1", "h1", "/mnt/data");
            let ctx = test_ctx();
            let view = rt()
                .block_on(storage_mount_detail(
                    StorageMountDetailArgs {
                        id: Some("m-1".into()),
                        ..Default::default()
                    },
                    &ctx,
                ))
                .expect("detail ok");
            assert_eq!(view.id, "m-1");
            assert_eq!(view.target, "/mnt/data");
            assert_eq!(view.share.id, "sh-1");
        });
    }

    // ── storage_mount_list: host-scope filter ─────────────────────────────

    #[test]
    fn storage_mount_list_filters_by_host_and_lists_all() {
        with_db("ml_filter.db", || {
            seed_share();
            seed_mount("m-1", "h1", "/mnt/a");
            seed_mount("m-2", "h2", "/mnt/b");
            let ctx = test_ctx();
            // Unscoped: both placements.
            let all = rt()
                .block_on(storage_mount_list(StorageMountListArgs::default(), &ctx))
                .expect("list ok");
            assert_eq!(all.len(), 2);
            // Scoped to h2: only its placement.
            let scoped = rt()
                .block_on(storage_mount_list(
                    StorageMountListArgs {
                        host: Some("h2".into()),
                    },
                    &ctx,
                ))
                .expect("list ok");
            assert_eq!(scoped.len(), 1);
            assert_eq!(scoped[0].host.id, "h2");
        });
    }
}
