//! Managed-unit — the universal capability surface.
//!
//! See `docs/MANAGED-UNIT.md`. Five canonical verbs cover everything:
//! [`Verb::List`] (collection GET + query), [`Verb::Detail`] (item GET),
//! [`Verb::Create`] (provision, backup, exec, add), [`Verb::Update`] (start,
//! stop, restart, migrate, restore, configure, version-bump), [`Verb::Delete`]
//! (destroy, remove). Plugins declare typed arg/outcome schemas per verb per
//! kind; orca validates + routes generically. No domain concepts leak into core.
//!
//! Mirrors the other capability registries (trait + process-global
//! `LazyLock<RwLock<Vec<Arc<dyn ..>>>>` + `register_from_def` FFI proxy). Async
//! trait methods are hand-desugared to [`BoxFuture`] (no `async_trait` macro).
//! Everything on the surface is fully typed — no opaque `serde_json::Value`.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

// ── Identity ──────────────────────────────────────────────────────────────────

/// A unit's four-axis identity. `kind` is a free string — core never enumerates
/// or branches on it; plugins do.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct UnitId {
    /// Who exposes it (e.g. `proxmox@cluster-a`, `docker@host-b`, `local`).
    pub manager: String,
    /// `vm` / `lxc` / `container` / `service` / `tv_show` / … Free string.
    pub kind: String,
    /// Manager-native identifier (vmid, slug, library id, …).
    pub id: String,
    /// Human label.
    pub name: String,
}

impl UnitId {
    /// Compose a per-instance manager from a provider base name and an instance
    /// scope: `scoped_manager("proxmox", "cluster-a") == "proxmox@cluster-a"`.
    /// The inverse of [`UnitId::manager_scope`]. This `<base>@<scope>` convention
    /// is the one core routing ([`owner_of`]) recognises, so plugins must build
    /// per-instance managers through this rather than hand-formatting `@`.
    pub fn scoped_manager(base: &str, scope: &str) -> String {
        format!("{base}@{scope}")
    }

    /// Split this unit's manager into `(base, Some(scope))` for a per-instance
    /// manager (`proxmox@cluster-a`) or `(base, None)` for a bare one (`local`).
    /// The single parse point for the `@` convention — plugins read their
    /// instance name from here instead of re-implementing `strip_prefix`.
    pub fn manager_scope(&self) -> (&str, Option<&str>) {
        match self.manager.split_once('@') {
            Some((base, scope)) => (base, Some(scope)),
            None => (self.manager.as_str(), None),
        }
    }

    /// The provider base name of this unit's manager, ignoring any `@scope`.
    pub fn manager_base(&self) -> &str {
        self.manager_scope().0
    }
}

// ── Six canonical verbs ───────────────────────────────────────────────────────

/// The complete canonical verb vocabulary. Six verbs cover every domain:
///
/// - [`List`]   — GET collection with query params (search, filter, log tail, …)
/// - [`Detail`] — GET one item (unit state, metadata, logs with query params)
/// - [`Create`] — POST something new; fails if it already exists (provision, add)
/// - [`Update`] — PATCH existing state; fails if absent (start/stop/migrate/…)
/// - [`Delete`] — DELETE (destroy, remove)
/// - [`Upsert`] — PUT by key: create if absent, replace if present (idempotent
///   set-by-key, e.g. `config upsert`). Distinct from Create/Update precisely
///   because it does not care whether the item already exists.
///
/// The args carry all domain semantics; the verb is just the CRUD axis.
/// No kind is owned by core — kind strings are plugin-declared.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    List,
    Detail,
    Create,
    Update,
    Delete,
    Upsert,
}

impl Verb {
    /// The [`Verb`] a set of [`VerbArgs`] carries.
    pub fn of(args: &VerbArgs) -> Verb {
        match args {
            VerbArgs::List(_) => Verb::List,
            VerbArgs::Detail(_) => Verb::Detail,
            VerbArgs::Create(_) => Verb::Create,
            VerbArgs::Update(_) => Verb::Update,
            VerbArgs::Delete(_) => Verb::Delete,
            VerbArgs::Upsert(_) => Verb::Upsert,
        }
    }
}

// ── Typed verb payloads ───────────────────────────────────────────────────────

/// Query parameters for [`Verb::List`] and [`Verb::Detail`].
/// Common fields typed here; plugin-specific filters declared via [`VerbDecl::args_schema`]
/// and validated at the boundary before the args string reaches the plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct QueryArgs {
    /// Free-text search / filter (search media, grep logs, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// Content-kind filter (`tv_show`, `movie`, `vm`, …). None = all kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Max items / log lines to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Pagination offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// Plugin-declared extra filter fields, schema-validated before passing.
    /// Carried as a JSON string so contract stays dep-free; validated by orca
    /// against the plugin's declared args_schema at the system boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
}

/// Args for [`Verb::List`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListArgs {
    #[serde(default)]
    pub query: QueryArgs,
}

/// Args for [`Verb::Detail`] — identify the item + optional query (log tail, …).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DetailArgs {
    pub id: UnitId,
    #[serde(default)]
    pub query: QueryArgs,
}

/// Args for [`Verb::Create`] — what kind of thing to create + plugin-shaped payload.
/// `action` names the create variant (`provision`, `backup`, `exec`, `add`, …);
/// `payload` is schema-validated JSON for that action, declared by the plugin.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateArgs {
    /// Discriminates the create variant. Plugin declares supported actions via
    /// [`VerbDecl::actions`].
    pub action: String,
    /// Schema-validated payload for this action (typed by the plugin's declared
    /// schema). Carried as a JSON string across the FFI boundary; orca validates
    /// before forwarding. `None` = no payload (e.g. `backup` with defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

/// Args for [`Verb::Update`] — identify the target + what update to apply.
/// `action` names the update variant (`start`, `stop`, `restart`, `migrate`,
/// `restore`, `configure`, `bump`, …).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub id: UnitId,
    /// Discriminates the update variant.
    pub action: String,
    /// Schema-validated payload (start has none; migrate carries a target UnitId;
    /// configure carries a config document; restore carries a BackupArtifact id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

/// Args for [`Verb::Delete`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteArgs {
    pub id: UnitId,
}

/// Args for [`Verb::Upsert`] — set an item by key, create-or-replace.
/// `id` is the natural key of the item; `action` names the upsert variant when a
/// kind supports more than one (`set`, …); `payload` is schema-validated JSON for
/// the new/replacement state. Unlike [`Verb::Create`]/[`Verb::Update`], an upsert
/// succeeds whether or not the item already exists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpsertArgs {
    pub id: UnitId,
    /// Discriminates the upsert variant. Plugin declares supported actions via
    /// [`VerbDecl::actions`]; defaults to `set` for single-variant kinds.
    #[serde(default = "default_upsert_action")]
    pub action: String,
    /// Schema-validated JSON payload for the new/replacement state (typed by the
    /// plugin's declared schema). Carried as a JSON string across the FFI boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

fn default_upsert_action() -> String {
    "set".to_string()
}

/// Typed args for one canonical verb. The variant IS the verb ([`Verb::of`]).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "verb", content = "args")]
pub enum VerbArgs {
    List(ListArgs),
    Detail(DetailArgs),
    Create(CreateArgs),
    Update(UpdateArgs),
    Delete(DeleteArgs),
    Upsert(UpsertArgs),
}

// ── Typed outcomes ────────────────────────────────────────────────────────────

/// One contributing source of a (possibly deduplicated) unit — the manager that
/// reported it and the locality of the path it was reached over. A unit seen by
/// several managers (e.g. every node of a PVE cluster, or both a container
/// runtime and its orchestrator) collapses to one [`ItemOutcome`] carrying every
/// source, so nothing is lost and the router can later pick the cheapest path.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct UnitSource {
    /// The reporting manager (e.g. `proxmox@host-d`, `docker@host-b`).
    pub manager: String,
    /// Locality class of the path this source was reached over (`fqdn` / `lan` /
    /// `tailscale`, provider-defined). `None` when the provider doesn't tag it.
    /// Consumed by the fewest-hop mutation router.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
}

/// Locality tier of a source's reach path — the network-distance component of
/// routing cost. Lower is cheaper: a unix/local socket beats a same-subnet LAN
/// hop, which beats a routed/DNS or overlay path. Unknown labels sort last so a
/// tagged path is always preferred over an untagged one.
pub fn locality_tier(locality: Option<&str>) -> u8 {
    match locality {
        Some("local") => 0,
        Some("lan") => 1,
        Some("fqdn") | Some("dns") | Some("tailscale") | Some("remote") => 2,
        _ => 3,
    }
}

/// Total routing cost for reaching a unit through one source:
/// `locality_tier + peer_hops`. `peer_hops` is 0 when the source's manager runs
/// on *this* orca and +1 per mesh hop to reach a peer that owns it (the caller
/// supplies it from pod state). Lower is cheaper; ties are broken on latency by
/// [`cheapest_source`].
pub fn source_cost(src: &UnitSource, peer_hops: u8) -> u32 {
    locality_tier(src.locality.as_deref()) as u32 + peer_hops as u32
}

/// Pick the cheapest source to route a Detail/Update/Delete over. `is_local`
/// answers whether this orca owns a manager (→ `peer_hops` 0 vs 1); `latency_ms`
/// breaks cost ties (lower wins; `None` sorts last). Returns `None` only for an
/// empty slice. The caller iterates the remaining sources in cost order on
/// failure, which is why nothing is ever dropped from `sources`.
pub fn cheapest_source(
    sources: &[UnitSource],
    is_local: impl Fn(&str) -> bool,
    latency_ms: impl Fn(&str) -> Option<u64>,
) -> Option<&UnitSource> {
    sources.iter().min_by(|a, b| {
        let ca = source_cost(a, if is_local(&a.manager) { 0 } else { 1 });
        let cb = source_cost(b, if is_local(&b.manager) { 0 } else { 1 });
        ca.cmp(&cb).then_with(|| {
            let la = latency_ms(&a.manager).unwrap_or(u64::MAX);
            let lb = latency_ms(&b.manager).unwrap_or(u64::MAX);
            la.cmp(&lb)
        })
    })
}

/// A single item returned by [`Verb::Detail`] or a [`Verb::Create`]/[`Verb::Update`]
/// that produces one resource. `payload` is schema-validated JSON from the plugin.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ItemOutcome {
    pub id: UnitId,
    /// Schema-validated JSON payload; shape declared by the plugin's response schema.
    pub payload: String,
    /// Provider-supplied stable identity used to deduplicate the same real thing
    /// reported by multiple managers. `None` falls back to `manager/kind/id`,
    /// which never collides across managers — so untouched providers are
    /// unaffected. Proxmox sets `cluster:<name>/<kind>/<vmid>` to collapse a
    /// cluster's guests (seen once per member node) into one item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    /// The resolved canonical identity — a uuidv7 (hyphenated string), filled in
    /// by core during the List merge from [`Self::canonical_key`] via the
    /// registered resolver ([`set_canonical_resolver`]). This is the pure,
    /// opaque identity references point at (dependents, delegated repairs); the
    /// descriptive `id` / `sources` are the routing axis and never *are* the
    /// identity. `None` when no resolver is installed (thin builds / unit tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
    /// Every manager that contributed this unit, accumulated during the List
    /// merge. Empty on a raw provider item; populated (≥1) after dedup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<UnitSource>,
    /// Datacenter / cluster this unit belongs to — the discovered cluster name,
    /// not hand config (proxmox: the PVE cluster name). Lets a consumer group
    /// units by datacenter even when orca doesn't run on the cluster's nodes.
    /// `None` for standalone/ungrouped units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datacenter: Option<String>,
}

impl ItemOutcome {
    /// A raw provider item — no canonical id, no sources yet (both are filled in
    /// by the List merge). This is the constructor providers should use so new
    /// dedup fields don't force call-site churn.
    pub fn new(id: UnitId, payload: String) -> Self {
        Self {
            id,
            payload,
            canonical: None,
            canonical_id: None,
            sources: Vec::new(),
            datacenter: None,
        }
    }

    /// Set the provider-supplied canonical identity (builder style).
    pub fn with_canonical(mut self, canonical: impl Into<String>) -> Self {
        self.canonical = Some(canonical.into());
        self
    }

    /// Set the datacenter / cluster name this unit belongs to (builder style).
    pub fn with_datacenter(mut self, datacenter: impl Into<String>) -> Self {
        self.datacenter = Some(datacenter.into());
        self
    }

    /// The key this item deduplicates on: its explicit [`Self::canonical`], or
    /// the fallback `manager/kind/id` (which never collides across managers, so
    /// providers that set no canonical are never merged with anything else).
    pub fn canonical_key(&self) -> String {
        self.canonical
            .clone()
            .unwrap_or_else(|| format!("{}/{}/{}", self.id.manager, self.id.kind, self.id.id))
    }
}

/// Collapse items that share a [`ItemOutcome::canonical_key`] into one, unioning
/// their [`UnitSource`]s. First occurrence wins for the representative id +
/// payload; every contributing manager is preserved as a source (an item with
/// none is given its own manager as an implicit source), so no provenance is
/// lost. Registered order is preserved.
///
/// Each merged item's [`ItemOutcome::canonical_id`] is resolved from its dedup
/// key via the registered [`resolve_canonical`] (a uuidv7 minted once per real
/// unit), so references can point at a pure, stable identity.
pub fn merge_by_canonical(items: Vec<ItemOutcome>) -> Vec<ItemOutcome> {
    use std::collections::HashMap;
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, ItemOutcome> = HashMap::new();
    for mut item in items {
        let key = item.canonical_key();
        if item.sources.is_empty() {
            item.sources.push(UnitSource {
                manager: item.id.manager.clone(),
                locality: None,
            });
        }
        match by_key.get_mut(&key) {
            None => {
                order.push(key.clone());
                by_key.insert(key, item);
            }
            Some(existing) => {
                for s in item.sources {
                    if !existing.sources.iter().any(|e| e.manager == s.manager) {
                        existing.sources.push(s);
                    }
                }
                // A later sighting may know the datacenter the first didn't.
                if existing.datacenter.is_none() {
                    existing.datacenter = item.datacenter;
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k).map(|item| (k, item)))
        .map(|(key, mut item)| {
            // Emit sources cheapest-first by locality so a consumer can route to
            // `sources[0]` and fall through the rest. Peer-hop + latency (which
            // need mesh state) are layered on by `cheapest_source` at route time.
            // Stable sort keeps registered order within a tier.
            item.sources
                .sort_by_key(|s| locality_tier(s.locality.as_deref()));
            // Point the representative id at the cheapest source's manager, so a
            // Detail/Update/Delete on the deduped unit routes through `owner_of`
            // over the lowest-cost path with no extra plumbing. For a single
            // source this is a no-op; for a cluster it picks the best member.
            if let Some(best) = item.sources.first() {
                item.id.manager = best.manager.clone();
            }
            // Resolve the pure canonical identity from the DEDUP key — captured
            // before the manager rewrite above, which would otherwise change the
            // composite-fallback coordinates. `None` when no resolver installed.
            item.canonical_id = resolve_canonical(&key);
            item
        })
        .collect()
}

/// Resolver mapping a unit's dedup key ([`ItemOutcome::canonical_key`]) to its
/// canonical uuidv7 identity (hyphenated string). Core installs this at daemon
/// boot, backed by the persisted `unit_identity` registry (mint-once, stable
/// after). Thin builds / unit tests leave it unset — [`resolve_canonical`] then
/// returns `None` and identity resolution is simply skipped.
type CanonicalResolver = dyn Fn(&str) -> Option<String> + Send + Sync;
static CANONICAL_RESOLVER: RwLock<Option<Arc<CanonicalResolver>>> = RwLock::new(None);

/// Install the canonical-identity resolver (called once at daemon boot).
/// Idempotent — a later call replaces the resolver.
pub fn set_canonical_resolver(resolver: Arc<CanonicalResolver>) {
    *CANONICAL_RESOLVER
        .write()
        .expect("canonical resolver poisoned") = Some(resolver);
}

/// Resolve a dedup key to its canonical uuidv7 identity, or `None` when no
/// resolver is installed.
pub fn resolve_canonical(key: &str) -> Option<String> {
    CANONICAL_RESOLVER
        .read()
        .expect("canonical resolver poisoned")
        .as_ref()
        .and_then(|f| f(key))
}

/// Group units by [`ItemOutcome::datacenter`], preserving first-seen order both
/// of the datacenters and of the units within each. `None` is the ungrouped
/// bucket. Lets `unit.list` consumers render a datacenter → units tree without
/// re-deriving the grouping.
pub fn group_by_datacenter(items: Vec<ItemOutcome>) -> Vec<(Option<String>, Vec<ItemOutcome>)> {
    let mut order: Vec<Option<String>> = Vec::new();
    let mut buckets: std::collections::HashMap<Option<String>, Vec<ItemOutcome>> =
        std::collections::HashMap::new();
    for item in items {
        let dc = item.datacenter.clone();
        if !buckets.contains_key(&dc) {
            order.push(dc.clone());
        }
        buckets.entry(dc).or_default().push(item);
    }
    order
        .into_iter()
        .map(|dc| {
            let units = buckets.remove(&dc).unwrap_or_default();
            (dc, units)
        })
        .collect()
}

/// A collection returned by [`Verb::List`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ItemsOutcome {
    pub items: Vec<ItemOutcome>,
    /// Total count before limit/offset (for pagination).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Result of a mutating verb that doesn't return a resource
/// ([`Verb::Update`] start/stop/restart/…, [`Verb::Delete`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ActionOutcome {
    pub changed: bool,
    #[serde(default)]
    pub message: String,
}

/// Typed outcome of a verb. Callers match the variant for the verb they issued.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "value")]
pub enum VerbOutcome {
    /// [`Verb::List`] result.
    Items(ItemsOutcome),
    /// [`Verb::Detail`] result, or a Create/Update that returns the resource.
    Item(ItemOutcome),
    /// [`Verb::Update`] / [`Verb::Delete`] / [`Verb::Create`] with no returned resource.
    Action(ActionOutcome),
}

// ── Declarations (plugin declares HOW) ────────────────────────────────────────

/// One action variant a plugin declares for [`Verb::Create`] or [`Verb::Update`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDecl {
    /// Action name (`start`, `stop`, `provision`, `backup`, `exec`, `add`, …).
    pub action: String,
    /// JSON Schema for the action's payload. `None` = no payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_schema: Option<Schema>,
    /// JSON Schema for the action's response payload. `None` = `ActionOutcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Schema>,
}

/// A declared verb + its typed schema surface. List/Detail carry query schemas;
/// Create/Update carry action rosters; Delete carries no schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerbDecl {
    pub verb: Verb,
    /// For [`Verb::List`] / [`Verb::Detail`]: extra query param schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_schema: Option<Schema>,
    /// For [`Verb::Create`] / [`Verb::Update`]: the actions this kind supports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionDecl>,
}

/// Canonical [`Verb::Update`] action name for resizing a unit's compute
/// resources (`Update { action: "set_resources" }`, carrying a
/// [`SetResourcesPayload`]). Runtime providers (hypervisor, container runtime, …)
/// advertise it via [`ActionDecl::set_resources`] so a caller resizes any unit
/// the same way, without knowing which runtime backs it.
pub const ACTION_SET_RESOURCES: &str = "set_resources";

/// Typed payload for [`ACTION_SET_RESOURCES`] — grow or adjust a managed unit's
/// compute resources. Every field is optional; only the present ones are applied.
/// The runtime provider validates each against host capacity before applying and
/// may clamp rather than fail (see [`SetResourcesResult::clamped`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SetResourcesPayload {
    /// New memory ceiling in MiB. `None` = leave unchanged. Note a RAM-backed
    /// scratch mount (tmpfs) counts against this ceiling, so a provider growing
    /// memory to back a larger tmpfs must keep the mount cap below the ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    /// New CPU core / vcpu count. `None` = leave unchanged. Providers must respect
    /// platform constraints (e.g. a hypervisor's `vcpus ≤ sockets × cores`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// Grow the unit's primary disk to this size in GiB. Grow-only — providers
    /// reject a shrink. `None` = leave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u64>,
}

/// Typed response for [`ACTION_SET_RESOURCES`]: the values in effect after
/// applying, so a caller can confirm what actually changed — a provider may clamp
/// a request down to host limits rather than fail it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SetResourcesResult {
    /// Memory ceiling in MiB now in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    /// CPU core / vcpu count now in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// Primary disk size in GiB now in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u64>,
    /// True if any requested value was clamped/adjusted to fit host limits.
    #[serde(default)]
    pub clamped: bool,
}

impl ActionDecl {
    /// An action with no payload/response schema — the common case for
    /// lifecycle verbs (`start`, `stop`, …) whose semantics are the action name.
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            payload_schema: None,
            response_schema: None,
        }
    }

    /// The canonical [`ACTION_SET_RESOURCES`] update action, wired with its typed
    /// [`SetResourcesPayload`] / [`SetResourcesResult`] schemas. A runtime provider
    /// adds this to its [`VerbDecl::update`] roster to advertise it can resize a
    /// unit's compute resources.
    pub fn set_resources() -> Self {
        Self::with_schemas(
            ACTION_SET_RESOURCES,
            Some(schemars::schema_for!(SetResourcesPayload)),
            Some(schemars::schema_for!(SetResourcesResult)),
        )
    }

    /// An action carrying typed payload and/or response schemas (e.g. a
    /// `provision` create that takes a typed body and returns a typed result).
    pub fn with_schemas(
        action: impl Into<String>,
        payload_schema: Option<Schema>,
        response_schema: Option<Schema>,
    ) -> Self {
        Self {
            action: action.into(),
            payload_schema,
            response_schema,
        }
    }
}

impl VerbDecl {
    pub fn list() -> Self {
        Self {
            verb: Verb::List,
            query_schema: None,
            actions: vec![],
        }
    }
    pub fn detail() -> Self {
        Self {
            verb: Verb::Detail,
            query_schema: None,
            actions: vec![],
        }
    }
    pub fn delete() -> Self {
        Self {
            verb: Verb::Delete,
            query_schema: None,
            actions: vec![],
        }
    }
    /// A [`Verb::Update`] declaring the action roster it supports (lifecycle
    /// verbs like `start`/`stop`/`restart`, config patches, …).
    pub fn update(actions: Vec<ActionDecl>) -> Self {
        Self {
            verb: Verb::Update,
            query_schema: None,
            actions,
        }
    }
    /// A [`Verb::Create`] declaring the action roster it supports (`provision`,
    /// `backup`, `add`, …), each typically carrying a payload/response schema.
    pub fn create(actions: Vec<ActionDecl>) -> Self {
        Self {
            verb: Verb::Create,
            query_schema: None,
            actions,
        }
    }
    /// A [`Verb::Upsert`] declaring the action roster it supports (typically the
    /// single `set` action). Create-or-replace by key; does not care whether the
    /// item already exists.
    pub fn upsert(actions: Vec<ActionDecl>) -> Self {
        Self {
            verb: Verb::Upsert,
            query_schema: None,
            actions,
        }
    }
}

/// One kind's declared surface. A provider returns many of these — Sonarr
/// returns `[tv_show, season, episode]`; proxmox returns `[vm, lxc, host]`.
/// No kind is owned by a plugin; any provider may declare any kind string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindDeclaration {
    /// Free kind string (`vm`, `lxc`, `tv_show`, `movie`, `service`, …).
    pub kind: String,
    pub verbs: Vec<VerbDecl>,
    /// This kind's minimal, restore-sufficient state (the generalization of
    /// `service`'s `data_paths()`). `None` = the kind declares no state to core;
    /// a provider may still implement the [`ACTION_BACKUP`] / [`ACTION_RESTORE`]
    /// actions itself (proxmox drives `vzdump` regardless), but declaring a spec
    /// makes the minimal state explicit and inspectable for the scheduler and the
    /// backup-target layer. See [`crate::backup::BackupSpec`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_spec: Option<crate::backup::BackupSpec>,
}

impl KindDeclaration {
    /// A kind declaring `verbs` and no backup spec. Use [`Self::with_backup_spec`]
    /// to attach one. Keeps existing call sites terse and future field additions
    /// from breaking them.
    pub fn new(kind: impl Into<String>, verbs: Vec<VerbDecl>) -> Self {
        Self {
            kind: kind.into(),
            verbs,
            backup_spec: None,
        }
    }

    /// Declare this kind's minimal state.
    pub fn with_backup_spec(mut self, spec: crate::backup::BackupSpec) -> Self {
        self.backup_spec = Some(spec);
        self
    }
}

/// One unit/resource currently exposed by a provider (for enumerable things).
/// Non-enumerable surfaces (pure query-based) return no descriptors and are
/// reached only via [`Verb::List`] / [`Verb::Create`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnitDescriptor {
    pub id: UnitId,
    /// Capability gate: exactly the verbs this unit supports.
    pub verbs: Vec<Verb>,
    /// Nesting: a container's parent is its host; a VM's parent is its proxmox node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<UnitId>,
}

// ── Provider trait ────────────────────────────────────────────────────────────

/// A source of managed units/resources. One plugin may register multiple
/// providers (one per resource domain). Each provider declares any number of
/// kind surfaces. Async methods return [`BoxFuture`].
pub trait UnitProvider: Send + Sync {
    /// Provider/registry name (registry key; replace-in-place on re-register).
    fn name(&self) -> &str;

    /// Declare all kind surfaces this provider implements. Sync + cheap.
    fn declarations(&self) -> Vec<KindDeclaration>;

    /// Enumerate units for lifecycle-managed kinds (VMs, containers, services).
    /// Pure query-based providers (media libraries) return `Ok(vec![])`.
    fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>>;

    /// Perform a canonical verb. The verb is encoded in `args` ([`Verb::of`]).
    fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>>;
}

// ── Registry ──────────────────────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn UnitProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register_provider(provider: Arc<dyn UnitProvider>) {
    let mut g = GLOBAL.write().expect("unit registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

pub fn providers() -> Vec<Arc<dyn UnitProvider>> {
    GLOBAL.read().expect("unit registry poisoned").clone()
}

pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("unit registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

pub async fn all_units() -> Vec<UnitDescriptor> {
    let mut out = Vec::new();
    for p in providers() {
        if let Ok(units) = p.units().await {
            out.extend(units);
        }
    }
    out
}

// ── Host-side catalog + routing ─────────────────────────────────────────────────
//
// The catalog is the single source of truth for what the whole system exposes
// at runtime. MCP (`tools/list`), the HTTP OpenAPI document, and the CLI
// (`--help`) are all generated from [`catalog()`] — never hand-maintained.
// Adding a plugin therefore self-enriches every surface with no code change.

/// One kind surface, tagged with the provider that exposes it. The typed
/// [`VerbDecl`]s (with their payload/response [`Schema`]s) are what the OpenAPI
/// spec, MCP input schemas, and CLI arg hints are built from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Registry key of the provider exposing this kind (`docker`, `proxmox`, …).
    pub provider: String,
    /// The kind string (`container`, `vm`, `tv_show`, …).
    pub kind: String,
    /// Every verb this provider implements for this kind, with typed schemas.
    pub verbs: Vec<VerbDecl>,
}

/// The complete runtime surface: every kind every registered provider exposes.
/// Rebuilt on demand so it always reflects currently-loaded plugins.
pub fn catalog() -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    for p in providers() {
        let provider = p.name().to_string();
        for decl in p.declarations() {
            out.push(CatalogEntry {
                provider: provider.clone(),
                kind: decl.kind,
                verbs: decl.verbs,
            });
        }
    }
    out
}

/// Providers that declare a given kind. Empty when nothing exposes it.
pub fn providers_for_kind(kind: &str) -> Vec<Arc<dyn UnitProvider>> {
    providers()
        .into_iter()
        .filter(|p| p.declarations().iter().any(|d| d.kind == kind))
        .collect()
}

/// The provider that owns a unit id. A provider `p` owns `id` when the id's
/// `manager` is exactly `p.name()` or is `"{p.name()}@…"` (per-endpoint managers
/// like `proxmox@cluster-a` all belong to the `proxmox` provider).
pub fn owner_of(id: &UnitId) -> Option<Arc<dyn UnitProvider>> {
    let base = id.manager_base();
    providers().into_iter().find(|p| p.name() == base)
}

/// Route a verb to the right provider(s) and return the merged outcome.
///
/// - [`Verb::List`] fans out to every provider (or only those declaring
///   `query.kind` when set) and merges the items.
/// - [`Verb::Detail`] / [`Verb::Update`] / [`Verb::Delete`] / [`Verb::Upsert`]
///   route to the single provider that [`owner_of`] the target id.
/// - [`Verb::Create`] has no existing target to derive an owner from — callers
///   must pick the provider explicitly via [`dispatch_to`].
pub async fn dispatch(args: VerbArgs) -> Result<VerbOutcome> {
    match args {
        VerbArgs::List(l) => {
            let targets = match l.query.kind.as_deref() {
                Some(kind) => providers_for_kind(kind),
                None => providers(),
            };
            let mut merged = ItemsOutcome::default();
            for p in targets {
                match p.invoke(VerbArgs::List(l.clone())).await {
                    Ok(VerbOutcome::Items(items)) => {
                        // Per-provider totals are dropped: after cross-manager
                        // dedup the only honest count is the merged item count,
                        // recomputed below.
                        merged.items.extend(items.items);
                    }
                    // A misbehaving provider returning a non-list outcome for a
                    // broad List must not sink the whole fleet-wide query — warn
                    // and skip it, exactly like an outright error below. (Also
                    // removes a cross-test flake: the process-global registry is
                    // shared, so a concurrently-registered provider answering
                    // List with an Action outcome would otherwise abort dispatch.)
                    Ok(other) => {
                        tracing::warn!(
                            provider = %p.name(),
                            "unit List fan-out: provider returned non-list outcome {other:?}; skipping"
                        );
                        continue;
                    }
                    // A single provider failing a broad list must not sink the
                    // whole query — skip it and keep merging the rest. Log at
                    // warn so the failure is observable: a silent skip makes a
                    // misconfigured/erroring provider look like it simply has no
                    // units, which is indistinguishable from success.
                    Err(e) => {
                        tracing::warn!(
                            provider = %p.name(),
                            error = %format!("{e:#}"),
                            "unit List fan-out: provider errored; skipping"
                        );
                        continue;
                    }
                }
            }
            // Collapse the same real unit reported by multiple managers (every
            // node of a cluster, a container seen by runtime + orchestrator, …)
            // into one item carrying all sources. Providers that set no
            // canonical id dedup on `manager/kind/id`, which never collides
            // cross-manager, so this is a no-op for them.
            merged.items = merge_by_canonical(merged.items);
            merged.total = Some(merged.items.len() as u64);
            Ok(VerbOutcome::Items(merged))
        }
        VerbArgs::Detail(d) => route_targeted(&d.id.clone(), VerbArgs::Detail(d)).await,
        VerbArgs::Update(u) => route_targeted(&u.id.clone(), VerbArgs::Update(u)).await,
        VerbArgs::Delete(d) => route_targeted(&d.id.clone(), VerbArgs::Delete(d)).await,
        // Upsert carries the target id: route by its manager (owner_of resolves by
        // manager name, so create-if-absent works even before the item exists).
        VerbArgs::Upsert(u) => route_targeted(&u.id.clone(), VerbArgs::Upsert(u)).await,
        VerbArgs::Create(_) => Err(anyhow::anyhow!(
            "Create has no target to route from; call dispatch_to(provider, args)"
        )),
    }
}

async fn route_targeted(id: &UnitId, args: VerbArgs) -> Result<VerbOutcome> {
    let provider = owner_of(id).ok_or_else(|| {
        anyhow::anyhow!(
            "no provider owns unit '{}' (manager '{}')",
            id.id,
            id.manager
        )
    })?;
    provider.invoke(args).await
}

/// Route a verb to a named provider explicitly. Used for [`Verb::Create`] (which
/// has no existing id to derive an owner from) and for callers that already know
/// the provider. Errors when no provider by that name is registered.
pub async fn dispatch_to(provider: &str, args: VerbArgs) -> Result<VerbOutcome> {
    let p = providers()
        .into_iter()
        .find(|p| p.name() == provider)
        .ok_or_else(|| anyhow::anyhow!("no unit provider named '{provider}'"))?;
    p.invoke(args).await
}

// ── Update-with-backup guard (MINIMAL-BACKUP.md §4.3) ──────────────────────────
//
// The centralized, gap-free replacement for per-host "backup before update"
// shell wrappers: before a mutating verb runs, the same unit's `backup` action
// is dispatched, and a backup failure ABORTS the mutation. Backup and restore
// are ordinary managed-unit actions (`Update { action: "backup" | "restore" }`),
// routable by unit id like any other verb — providers declare and implement
// them; core only names them and sequences the guard.

/// Canonical action name for taking a unit's minimal backup
/// (`Update { action: "backup" }`), producing a [`crate::BackupRef`].
pub const ACTION_BACKUP: &str = "backup";
/// Canonical action name for restoring a unit from a backup
/// (`Update { action: "restore" }` carrying a [`crate::RestorePayload`]).
pub const ACTION_RESTORE: &str = "restore";

/// No-data lifecycle / read actions that never warrant a pre-mutation backup —
/// they change no persistent state, and `backup`/`restore` themselves must be
/// excluded (guarding `backup` would recurse; a `restore` is already recovery).
const UNGUARDED_ACTIONS: &[&str] = &[
    "start",
    "stop",
    "restart",
    "shutdown",
    "reboot",
    "status",
    ACTION_BACKUP,
    ACTION_RESTORE,
];

/// Whether `args` mutates persistent state such that a pre-mutation backup is
/// warranted. Only verbs that target an existing unit are guarded (`Update` /
/// `Upsert` / `Delete`); `Create` has no prior state to protect, and read verbs
/// (`List` / `Detail`) and no-data lifecycle transitions never are.
pub fn action_is_guarded(args: &VerbArgs) -> bool {
    match args {
        VerbArgs::Update(u) => !UNGUARDED_ACTIONS.contains(&u.action.as_str()),
        VerbArgs::Upsert(u) => !UNGUARDED_ACTIONS.contains(&u.action.as_str()),
        VerbArgs::Delete(_) => true,
        _ => false,
    }
}

/// The target unit id of a guarded verb (`Update` / `Upsert` / `Delete` all
/// carry one; other verbs return `None`).
fn guarded_target(args: &VerbArgs) -> Option<&UnitId> {
    match args {
        VerbArgs::Update(u) => Some(&u.id),
        VerbArgs::Upsert(u) => Some(&u.id),
        VerbArgs::Delete(d) => Some(&d.id),
        _ => None,
    }
}

/// Run a mutating verb behind the pre-mutation backup guard.
///
/// When `back_up` is true and `args` is a [guarded](action_is_guarded) verb, the
/// same unit's [`ACTION_BACKUP`] action is dispatched first; **if the backup
/// fails, the mutation is aborted** and the backup error is returned. Otherwise
/// the mutation proceeds. Non-guarded verbs (and `back_up == false`) pass
/// straight through to [`dispatch`].
///
/// `back_up` is resolved by the caller from the unit's [`crate::BackupPolicy`] /
/// [`crate::BackupGate`] (see [`crate::BackupGate::decide`]) plus any interactive
/// consent — the contract layer stays free of prompting and policy storage.
pub async fn dispatch_guarded(args: VerbArgs, back_up: bool) -> Result<VerbOutcome> {
    if back_up
        && action_is_guarded(&args)
        && let Some(id) = guarded_target(&args)
    {
        let target = id.clone();
        let backup = VerbArgs::Update(UpdateArgs {
            id: target.clone(),
            action: ACTION_BACKUP.to_string(),
            payload: None,
        });
        dispatch(backup).await.map_err(|e| {
            anyhow::anyhow!(
                "pre-mutation backup of unit '{}' failed; aborting {:?} — {e:#}",
                target.id,
                Verb::of(&args),
            )
        })?;
    }
    dispatch(args).await
}

// ── FFI bridge ────────────────────────────────────────────────────────────────

// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

pub const UNITS_OP: &str = "units";
pub const DECLARATIONS_OP: &str = "declarations";
pub const INVOKE_OP: &str = "invoke";

/// Wire payload for [`INVOKE_OP`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeCall {
    pub args: VerbArgs,
}

// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    let declarations = match invoke(DECLARATIONS_OP, "{}".to_string()) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    register_provider(Arc::new(FfiUnitProvider {
        name,
        invoke,
        declarations,
    }));
    Ok(())
}

// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct FfiUnitProvider {
    name: String,
    invoke: InvokeThunk,
    declarations: Vec<KindDeclaration>,
}

#[cfg(feature = "in-process")]
impl UnitProvider for FfiUnitProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn declarations(&self) -> Vec<KindDeclaration> {
        self.declarations.clone()
    }

    fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(UNITS_OP, "{}".to_string()))
                .await
                .map_err(|e| anyhow::anyhow!("unit '{name}' units task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("unit '{name}' units failed: {e}"))?;
            serde_json::from_str(&out)
                .map_err(|e| anyhow::anyhow!("unit '{name}' returned invalid units JSON: {e}"))
        })
    }

    fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let call = InvokeCall { args };
        Box::pin(async move {
            let args_json = serde_json::to_string(&call)
                .map_err(|e| anyhow::anyhow!("unit '{name}' encode invoke args: {e}"))?;
            let out = tokio::task::spawn_blocking(move || invoke(INVOKE_OP, args_json))
                .await
                .map_err(|e| anyhow::anyhow!("unit '{name}' invoke task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("unit '{name}' invoke failed: {e}"))?;
            serde_json::from_str(&out)
                .map_err(|e| anyhow::anyhow!("unit '{name}' returned invalid outcome JSON: {e}"))
        })
    }
}

// ── Plugin-side dispatch ──────────────────────────────────────────────────────

pub async fn dispatch_op(
    provider: &dyn UnitProvider,
    op: &str,
    args_json: &str,
) -> std::result::Result<String, String> {
    match op {
        DECLARATIONS_OP => {
            serde_json::to_string(&provider.declarations()).map_err(|e| e.to_string())
        }
        UNITS_OP => {
            let units = provider.units().await.map_err(|e| format!("{e:#}"))?;
            serde_json::to_string(&units).map_err(|e| e.to_string())
        }
        INVOKE_OP => {
            let call: InvokeCall =
                serde_json::from_str(args_json).map_err(|e| format!("decode invoke args: {e}"))?;
            let outcome = provider
                .invoke(call.args)
                .await
                .map_err(|e| format!("{e:#}"))?;
            serde_json::to_string(&outcome).map_err(|e| e.to_string())
        }
        other => Err(format!("unknown unit op: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_declaration_builder_and_backup_spec_serde() {
        use crate::backup::{BackupSpec, BackupStrategy};
        // `new` leaves the spec absent, and an absent spec is skipped in JSON so
        // every existing declaration serializes byte-identically.
        let bare = KindDeclaration::new("container", vec![VerbDecl::list()]);
        assert!(bare.backup_spec.is_none());
        let json = serde_json::to_string(&bare).unwrap();
        assert!(
            !json.contains("backup_spec"),
            "absent spec must not serialize"
        );

        // `with_backup_spec` attaches and round-trips.
        let spec = BackupSpec::paths(["/opt/stacks/app".to_string()]);
        let decl =
            KindDeclaration::new("stack", vec![VerbDecl::list()]).with_backup_spec(spec.clone());
        let round: KindDeclaration =
            serde_json::from_str(&serde_json::to_string(&decl).unwrap()).unwrap();
        assert_eq!(round.backup_spec.as_ref().unwrap().include, spec.include);
        assert_eq!(
            round.backup_spec.unwrap().strategies,
            vec![BackupStrategy::Paths]
        );
    }

    #[test]
    fn set_resources_action_decl_wires_typed_schemas() {
        let decl = ActionDecl::set_resources();
        assert_eq!(decl.action, ACTION_SET_RESOURCES);
        assert!(
            decl.payload_schema.is_some(),
            "set_resources must declare a payload schema"
        );
        assert!(
            decl.response_schema.is_some(),
            "set_resources must declare a response schema"
        );

        // Partial payload: only the present fields serialize (grow RAM only).
        let payload = SetResourcesPayload {
            memory_mib: Some(8192),
            ..Default::default()
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("memory_mib"));
        assert!(!json.contains("cores"), "absent fields must be skipped");
        let round: SetResourcesPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(round, payload);

        // It rides the ordinary Update verb, so the backup guard covers it.
        let args = VerbArgs::Update(UpdateArgs {
            id: UnitId {
                manager: "proxmox@cluster-a".into(),
                kind: "lxc".into(),
                id: "110".into(),
                name: "mediabox".into(),
            },
            action: ACTION_SET_RESOURCES.into(),
            payload: Some(json),
        });
        assert_eq!(Verb::of(&args), Verb::Update);
        assert!(action_is_guarded(&args));
    }

    #[test]
    fn verb_of_maps_every_variant() {
        assert_eq!(Verb::of(&VerbArgs::List(ListArgs::default())), Verb::List);
        assert_eq!(
            Verb::of(&VerbArgs::Create(CreateArgs {
                action: "provision".into(),
                payload: None,
            })),
            Verb::Create
        );
        assert_eq!(
            Verb::of(&VerbArgs::Update(UpdateArgs {
                id: UnitId {
                    manager: "test".into(),
                    kind: "vm".into(),
                    id: "1".into(),
                    name: "x".into(),
                },
                action: "start".into(),
                payload: None,
            })),
            Verb::Update
        );
    }

    #[cfg(feature = "in-process")]
    fn fake_thunk() -> InvokeThunk {
        Arc::new(|op: &str, args: String| match op {
            DECLARATIONS_OP => Ok(serde_json::to_string(&vec![KindDeclaration {
                kind: "vm".into(),
                backup_spec: None,
                verbs: vec![
                    VerbDecl::list(),
                    VerbDecl::detail(),
                    VerbDecl {
                        verb: Verb::Update,
                        query_schema: None,
                        actions: vec![
                            ActionDecl {
                                action: "start".into(),
                                payload_schema: None,
                                response_schema: None,
                            },
                            ActionDecl {
                                action: "stop".into(),
                                payload_schema: None,
                                response_schema: None,
                            },
                        ],
                    },
                    VerbDecl {
                        verb: Verb::Create,
                        query_schema: None,
                        actions: vec![ActionDecl {
                            action: "provision".into(),
                            payload_schema: None,
                            response_schema: None,
                        }],
                    },
                    VerbDecl::delete(),
                ],
            }])
            .unwrap()),
            UNITS_OP => Ok(serde_json::to_string(&vec![UnitDescriptor {
                id: UnitId {
                    manager: "fake@x".into(),
                    kind: "vm".into(),
                    id: "100".into(),
                    name: "web".into(),
                },
                verbs: vec![Verb::Detail, Verb::Update, Verb::Delete],
                parent: None,
            }])
            .unwrap()),
            INVOKE_OP => {
                let call: InvokeCall = serde_json::from_str(&args).unwrap();
                let out = match call.args {
                    VerbArgs::Update(u) => VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: u.action,
                    }),
                    _ => VerbOutcome::Action(ActionOutcome::default()),
                };
                Ok(serde_json::to_string(&out).unwrap())
            }
            other => Err(format!("unexpected op {other}")),
        })
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn ffi_provider_round_trips_units_and_invoke() {
        register_from_def("prov-unit-test-v2".into(), fake_thunk()).unwrap();

        let prov = providers()
            .into_iter()
            .find(|p| p.name() == "prov-unit-test-v2")
            .unwrap();
        assert_eq!(prov.declarations()[0].kind, "vm");

        let units = all_units().await;
        let u = units.iter().find(|u| u.id.id == "100").unwrap();
        assert!(u.verbs.contains(&Verb::Update));

        let outcome = prov
            .invoke(VerbArgs::Update(UpdateArgs {
                id: u.id.clone(),
                action: "start".into(),
                payload: None,
            }))
            .await
            .unwrap();
        match outcome {
            VerbOutcome::Action(a) => {
                assert!(a.changed);
                assert_eq!(a.message, "start");
            }
            other => panic!("expected action, got {other:?}"),
        }

        assert!(deregister_provider("prov-unit-test-v2"));
    }

    // ── Native mock provider for host-side routing tests ────────────────────────

    struct MockProvider {
        name: String,
        kinds: Vec<String>,
        unit_ids: Vec<UnitId>,
    }

    impl UnitProvider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            self.kinds
                .iter()
                .map(|k| KindDeclaration {
                    kind: k.clone(),
                    backup_spec: None,
                    verbs: vec![
                        VerbDecl::list(),
                        VerbDecl::detail(),
                        VerbDecl {
                            verb: Verb::Update,
                            query_schema: None,
                            actions: vec![ActionDecl {
                                action: "start".into(),
                                payload_schema: None,
                                response_schema: None,
                            }],
                        },
                    ],
                })
                .collect()
        }
        fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
            Box::pin(async move {
                Ok(self
                    .unit_ids
                    .iter()
                    .map(|id| UnitDescriptor {
                        id: id.clone(),
                        verbs: vec![Verb::Detail, Verb::Update],
                        parent: None,
                    })
                    .collect())
            })
        }
        fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
            let name = self.name.clone();
            let ids = self.unit_ids.clone();
            Box::pin(async move {
                match args {
                    VerbArgs::List(_) => Ok(VerbOutcome::Items(ItemsOutcome {
                        items: ids
                            .iter()
                            .map(|id| ItemOutcome::new(id.clone(), "{}".into()))
                            .collect(),
                        total: Some(ids.len() as u64),
                    })),
                    VerbArgs::Update(u) => Ok(VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: format!("{name}:{}", u.action),
                    })),
                    _ => Ok(VerbOutcome::Action(ActionOutcome::default())),
                }
            })
        }
    }

    fn mock(name: &str, kinds: &[&str], ids: Vec<UnitId>) -> Arc<dyn UnitProvider> {
        Arc::new(MockProvider {
            name: name.into(),
            kinds: kinds.iter().map(|s| s.to_string()).collect(),
            unit_ids: ids,
        })
    }

    fn uid(manager: &str, kind: &str, id: &str) -> UnitId {
        UnitId {
            manager: manager.into(),
            kind: kind.into(),
            id: id.into(),
            name: id.into(),
        }
    }

    #[test]
    fn verbdecl_constructors_set_verb_and_actions() {
        let a = ActionDecl::new("start");
        assert_eq!(a.action, "start");
        assert!(a.payload_schema.is_none() && a.response_schema.is_none());

        let u = VerbDecl::update(vec![ActionDecl::new("start"), ActionDecl::new("stop")]);
        assert_eq!(u.verb, Verb::Update);
        assert_eq!(u.actions.len(), 2);
        assert!(u.query_schema.is_none());

        let c = VerbDecl::create(vec![ActionDecl::with_schemas("provision", None, None)]);
        assert_eq!(c.verb, Verb::Create);
        assert_eq!(c.actions[0].action, "provision");
    }

    #[test]
    fn manager_scope_round_trips_and_splits() {
        // Compose → split is a round-trip.
        let m = UnitId::scoped_manager("proxmox", "cluster-a");
        assert_eq!(m, "proxmox@cluster-a");
        let scoped = uid(&m, "vm", "100");
        assert_eq!(scoped.manager_scope(), ("proxmox", Some("cluster-a")));
        assert_eq!(scoped.manager_base(), "proxmox");

        // A bare manager has no scope.
        let bare = uid("local", "service", "sshd");
        assert_eq!(bare.manager_scope(), ("local", None));
        assert_eq!(bare.manager_base(), "local");

        // Only the first `@` splits; scope may itself contain `@`.
        let odd = uid("docker@host@weird", "container", "x");
        assert_eq!(odd.manager_scope(), ("docker", Some("host@weird")));
        assert_eq!(odd.manager_base(), "docker");
    }

    #[test]
    fn merge_collapses_same_canonical_across_managers() {
        // The same cluster guest reported by three member-node managers.
        let items: Vec<ItemOutcome> = ["proxmox@host-d", "proxmox@host-b", "proxmox@host-c"]
            .iter()
            .map(|m| {
                ItemOutcome::new(uid(m, "lxc", "100"), "{}".into())
                    .with_canonical("cluster:cluster-a/lxc/100")
            })
            .collect();
        let merged = merge_by_canonical(items);
        assert_eq!(merged.len(), 1, "three sightings collapse to one unit");
        let managers: Vec<_> = merged[0]
            .sources
            .iter()
            .map(|s| s.manager.as_str())
            .collect();
        assert_eq!(
            managers,
            ["proxmox@host-d", "proxmox@host-b", "proxmox@host-c"]
        );
    }

    #[test]
    fn merge_resolves_canonical_id_from_dedup_key() {
        // With a resolver installed, each merged item carries the resolved pure
        // identity (uuidv7 in prod; a deterministic stand-in here), keyed on the
        // dedup key — not the descriptive/routing coordinates.
        set_canonical_resolver(std::sync::Arc::new(|k: &str| Some(format!("id-for-{k}"))));
        let items = vec![
            ItemOutcome::new(uid("proxmox@host-a", "lxc", "100"), "{}".into())
                .with_canonical("cluster:cluster-a/lxc/100"),
            ItemOutcome::new(uid("proxmox@host-a", "vm", "200"), "{}".into())
                .with_canonical("cluster:cluster-a/vm/200"),
        ];
        let merged = merge_by_canonical(items);
        assert_eq!(merged.len(), 2, "distinct units are not collapsed");
        assert_eq!(
            merged[0].canonical_id.as_deref(),
            Some("id-for-cluster:cluster-a/lxc/100"),
            "merged item carries the identity resolved from its dedup key"
        );
        let ids: Vec<_> = merged
            .iter()
            .filter_map(|i| i.canonical_id.clone())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "distinct dedup keys resolve to distinct ids"
        );
    }

    #[test]
    fn merge_keeps_distinct_canonicals_and_is_order_preserving() {
        let items = vec![
            ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
                .with_canonical("cluster:cluster-a/lxc/100"),
            ItemOutcome::new(uid("proxmox@host-d", "vm", "200"), "{}".into())
                .with_canonical("cluster:cluster-a/vm/200"),
        ];
        let merged = merge_by_canonical(items);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id.id, "100");
        assert_eq!(merged[1].id.id, "200");
    }

    #[test]
    fn merge_without_canonical_falls_back_to_manager_kind_id_no_collision() {
        // No canonical set: two different managers with the same kind+id must
        // NOT be merged (fallback key includes the manager).
        let items = vec![
            ItemOutcome::new(uid("docker@a", "container", "web"), "{}".into()),
            ItemOutcome::new(uid("docker@b", "container", "web"), "{}".into()),
        ];
        let merged = merge_by_canonical(items);
        assert_eq!(
            merged.len(),
            2,
            "distinct managers never collide on fallback key"
        );
        // Each carries exactly its own implicit self-source.
        assert_eq!(merged[0].sources.len(), 1);
        assert_eq!(merged[0].sources[0].manager, "docker@a");
    }

    fn src(manager: &str, locality: Option<&str>) -> UnitSource {
        UnitSource {
            manager: manager.into(),
            locality: locality.map(Into::into),
        }
    }

    #[test]
    fn locality_tier_orders_local_lan_remote_unknown() {
        assert!(locality_tier(Some("local")) < locality_tier(Some("lan")));
        assert!(locality_tier(Some("lan")) < locality_tier(Some("tailscale")));
        assert!(locality_tier(Some("fqdn")) < locality_tier(None));
        assert_eq!(
            locality_tier(Some("fqdn")),
            locality_tier(Some("tailscale"))
        );
    }

    #[test]
    fn cheapest_prefers_local_manager_over_lower_locality_peer() {
        // A LAN path on a peer (tier 1 + 1 hop = 2) loses to a remote-tier path
        // on THIS orca (tier 2 + 0 hops = 2)… tie → latency decides.
        let sources = vec![
            src("proxmox@peer", Some("lan")),
            src("proxmox@local", Some("fqdn")),
        ];
        let pick = cheapest_source(
            &sources,
            |m| m == "proxmox@local",
            |m| {
                if m == "proxmox@local" {
                    Some(5)
                } else {
                    Some(50)
                }
            },
        );
        assert_eq!(pick.unwrap().manager, "proxmox@local");
    }

    #[test]
    fn cheapest_all_local_picks_best_locality_then_latency() {
        let sources = vec![
            src("proxmox@host-d", Some("tailscale")),
            src("proxmox@host-b", Some("lan")),
            src("proxmox@host-c", Some("lan")),
        ];
        // host-d is tier 2; host-b/host-c tier 1 tie → lower latency (host-c) wins.
        let pick = cheapest_source(
            &sources,
            |_| true,
            |m| Some(if m == "proxmox@host-c" { 2 } else { 9 }),
        );
        assert_eq!(pick.unwrap().manager, "proxmox@host-c");
    }

    #[test]
    fn cheapest_empty_is_none() {
        assert!(cheapest_source(&[], |_| true, |_| None).is_none());
    }

    #[test]
    fn merge_emits_sources_cheapest_first() {
        // Same unit reported over a tailscale path first, then LAN — merged
        // sources must come out LAN (cheaper) first.
        let mut a = ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
            .with_canonical("c/lxc/100");
        a.sources = vec![src("proxmox@host-d", Some("tailscale"))];
        let mut b = ItemOutcome::new(uid("proxmox@host-b", "lxc", "100"), "{}".into())
            .with_canonical("c/lxc/100");
        b.sources = vec![src("proxmox@host-b", Some("lan"))];
        let merged = merge_by_canonical(vec![a, b]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources[0].locality.as_deref(), Some("lan"));
    }

    #[test]
    fn merge_repoints_id_manager_to_cheapest_source() {
        // Same unit first seen via a tailscale manager, then a LAN one. The
        // deduped id must route over the LAN manager (cheapest).
        let mut a = ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
            .with_canonical("c/lxc/100");
        a.sources = vec![src("proxmox@host-d", Some("tailscale"))];
        let mut b = ItemOutcome::new(uid("proxmox@host-b", "lxc", "100"), "{}".into())
            .with_canonical("c/lxc/100");
        b.sources = vec![src("proxmox@host-b", Some("lan"))];
        let merged = merge_by_canonical(vec![a, b]);
        assert_eq!(merged[0].id.manager, "proxmox@host-b");
    }

    #[test]
    fn merge_unions_sources_without_duplicating_managers() {
        let items = vec![
            ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
                .with_canonical("c/lxc/100"),
            // Same manager reports it twice — must not double-count the source.
            ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
                .with_canonical("c/lxc/100"),
        ];
        let merged = merge_by_canonical(items);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources.len(), 1);
    }

    #[test]
    fn merge_adopts_datacenter_from_later_sighting() {
        // First sighting doesn't know the cluster; a later one does.
        let items = vec![
            ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
                .with_canonical("c/lxc/100"),
            ItemOutcome::new(uid("proxmox@host-b", "lxc", "100"), "{}".into())
                .with_canonical("c/lxc/100")
                .with_datacenter("cluster-a"),
        ];
        let merged = merge_by_canonical(items);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].datacenter.as_deref(), Some("cluster-a"));
    }

    #[test]
    fn group_by_datacenter_buckets_and_orders() {
        let items = vec![
            ItemOutcome::new(uid("proxmox@host-d", "lxc", "100"), "{}".into())
                .with_datacenter("cluster-a"),
            ItemOutcome::new(uid("docker@a", "container", "web"), "{}".into()),
            ItemOutcome::new(uid("proxmox@host-d", "vm", "200"), "{}".into())
                .with_datacenter("cluster-a"),
        ];
        let groups = group_by_datacenter(items);
        // cluster-a (first seen) then the ungrouped None bucket.
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0.as_deref(), Some("cluster-a"));
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, None);
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn catalog_tags_each_kind_with_its_provider() {
        register_provider(mock("cat-docker", &["container"], vec![]));
        register_provider(mock("cat-proxmox", &["vm", "lxc"], vec![]));

        let cat = catalog();
        let docker: Vec<_> = cat.iter().filter(|e| e.provider == "cat-docker").collect();
        let proxmox: Vec<_> = cat.iter().filter(|e| e.provider == "cat-proxmox").collect();
        assert_eq!(docker.len(), 1);
        assert_eq!(docker[0].kind, "container");
        assert_eq!(proxmox.len(), 2);
        assert!(proxmox.iter().any(|e| e.kind == "vm"));
        assert!(proxmox.iter().any(|e| e.kind == "lxc"));

        assert!(deregister_provider("cat-docker"));
        assert!(deregister_provider("cat-proxmox"));
    }

    #[test]
    fn owner_of_matches_bare_and_at_prefixed_managers() {
        register_provider(mock(
            "own-proxmox",
            &["vm"],
            vec![uid("own-proxmox@cluster-a", "vm", "100")],
        ));
        register_provider(mock(
            "own-local",
            &["service"],
            vec![uid("own-local", "service", "sshd")],
        ));

        // per-endpoint manager routes to the base provider
        let p = owner_of(&uid("own-proxmox@cluster-a", "vm", "100")).unwrap();
        assert_eq!(p.name(), "own-proxmox");
        // exact-match manager
        let p = owner_of(&uid("own-local", "service", "sshd")).unwrap();
        assert_eq!(p.name(), "own-local");
        // unknown manager → no owner
        assert!(owner_of(&uid("nobody@x", "vm", "1")).is_none());

        assert!(deregister_provider("own-proxmox"));
        assert!(deregister_provider("own-local"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_list_fans_out_and_merges() {
        register_provider(mock(
            "fan-a",
            &["vm"],
            vec![uid("fan-a", "vm", "1"), uid("fan-a", "vm", "2")],
        ));
        register_provider(mock("fan-b", &["vm"], vec![uid("fan-b", "vm", "3")]));
        register_provider(mock(
            "fan-c",
            &["container"],
            vec![uid("fan-c", "container", "x")],
        ));

        // broad list: every provider
        let out = dispatch(VerbArgs::List(ListArgs::default())).await.unwrap();
        let items = match out {
            VerbOutcome::Items(i) => i,
            other => panic!("expected items, got {other:?}"),
        };
        // at least our 4 (registry is process-global; other tests may add more)
        assert!(
            items
                .items
                .iter()
                .filter(|i| i.id.manager.starts_with("fan-"))
                .count()
                == 4
        );
        assert_eq!(items.total, Some(items.items.len() as u64));

        // kind-scoped list: only vm providers
        let out = dispatch(VerbArgs::List(ListArgs {
            query: QueryArgs {
                kind: Some("vm".into()),
                ..Default::default()
            },
        }))
        .await
        .unwrap();
        let items = match out {
            VerbOutcome::Items(i) => i,
            other => panic!("expected items, got {other:?}"),
        };
        let ours: Vec<_> = items
            .items
            .iter()
            .filter(|i| i.id.manager.starts_with("fan-"))
            .collect();
        assert_eq!(ours.len(), 3, "only vm units from fan-a/fan-b");
        assert!(ours.iter().all(|i| i.id.kind == "vm"));

        assert!(deregister_provider("fan-a"));
        assert!(deregister_provider("fan-b"));
        assert!(deregister_provider("fan-c"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_targeted_routes_to_owner() {
        register_provider(mock("rt-a", &["vm"], vec![uid("rt-a", "vm", "1")]));
        register_provider(mock("rt-b", &["vm"], vec![uid("rt-b", "vm", "2")]));

        let out = dispatch(VerbArgs::Update(UpdateArgs {
            id: uid("rt-b", "vm", "2"),
            action: "start".into(),
            payload: None,
        }))
        .await
        .unwrap();
        match out {
            VerbOutcome::Action(a) => assert_eq!(a.message, "rt-b:start"),
            other => panic!("expected action, got {other:?}"),
        }

        assert!(deregister_provider("rt-a"));
        assert!(deregister_provider("rt-b"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_targeted_unknown_owner_errors() {
        let err = dispatch(VerbArgs::Delete(DeleteArgs {
            id: uid("ghost@x", "vm", "999"),
        }))
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no provider owns"), "got: {err}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_create_requires_explicit_provider() {
        let err = dispatch(VerbArgs::Create(CreateArgs {
            action: "provision".into(),
            payload: None,
        }))
        .await
        .unwrap_err();
        assert!(err.to_string().contains("dispatch_to"), "got: {err}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_to_named_provider() {
        register_provider(mock("dt-proxmox", &["vm"], vec![]));

        let out = dispatch_to(
            "dt-proxmox",
            VerbArgs::Update(UpdateArgs {
                id: uid("dt-proxmox@c", "vm", "1"),
                action: "start".into(),
                payload: None,
            }),
        )
        .await
        .unwrap();
        match out {
            VerbOutcome::Action(a) => assert_eq!(a.message, "dt-proxmox:start"),
            other => panic!("expected action, got {other:?}"),
        }

        let err = dispatch_to("nonexistent-prov", VerbArgs::List(ListArgs::default()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no unit provider named"),
            "got: {err}"
        );

        assert!(deregister_provider("dt-proxmox"));
    }

    // ── Update-with-backup guard ────────────────────────────────────────────────

    #[test]
    fn guarded_classification() {
        let id = uid("g-prov", "lxc", "100");
        // Mutating verbs targeting an existing unit are guarded…
        assert!(action_is_guarded(&VerbArgs::Update(UpdateArgs {
            id: id.clone(),
            action: "configure".into(),
            payload: None,
        })));
        assert!(action_is_guarded(&VerbArgs::Delete(DeleteArgs {
            id: id.clone()
        })));
        assert!(action_is_guarded(&VerbArgs::Upsert(UpsertArgs {
            id: id.clone(),
            action: "set".into(),
            payload: None,
        })));
        // …but no-data lifecycle / read / backup-itself actions are not.
        for action in ["start", "stop", "reboot", ACTION_BACKUP, ACTION_RESTORE] {
            assert!(
                !action_is_guarded(&VerbArgs::Update(UpdateArgs {
                    id: id.clone(),
                    action: action.into(),
                    payload: None,
                })),
                "{action} must not be guarded"
            );
        }
        // Create has no prior state to protect; List is a read.
        assert!(!action_is_guarded(&VerbArgs::Create(CreateArgs {
            action: "provision".into(),
            payload: None,
        })));
        assert!(!action_is_guarded(&VerbArgs::List(ListArgs::default())));
    }

    // Records every action it is invoked with, in order, and optionally fails
    // the `backup` action to exercise abort-on-failure.
    struct RecordingProvider {
        name: String,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        fail_backup: bool,
    }

    impl UnitProvider for RecordingProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            vec![KindDeclaration {
                kind: "lxc".into(),
                backup_spec: None,
                verbs: vec![VerbDecl::update(vec![
                    ActionDecl::new(ACTION_BACKUP),
                    ActionDecl::new("configure"),
                ])],
            }]
        }
        fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
            let calls = self.calls.clone();
            let fail_backup = self.fail_backup;
            Box::pin(async move {
                if let VerbArgs::Update(u) = &args {
                    calls.lock().unwrap().push(u.action.clone());
                    if u.action == ACTION_BACKUP && fail_backup {
                        anyhow::bail!("backup storage unreachable");
                    }
                }
                Ok(VerbOutcome::Action(ActionOutcome {
                    changed: true,
                    message: "ok".into(),
                }))
            })
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn guard_backs_up_before_mutation() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        register_provider(Arc::new(RecordingProvider {
            name: "grd-a".into(),
            calls: calls.clone(),
            fail_backup: false,
        }));

        dispatch_guarded(
            VerbArgs::Update(UpdateArgs {
                id: uid("grd-a", "lxc", "100"),
                action: "configure".into(),
                payload: None,
            }),
            true,
        )
        .await
        .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![ACTION_BACKUP.to_string(), "configure".to_string()],
            "backup must run before the mutation, in that order"
        );
        assert!(deregister_provider("grd-a"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn guard_aborts_mutation_when_backup_fails() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        register_provider(Arc::new(RecordingProvider {
            name: "grd-b".into(),
            calls: calls.clone(),
            fail_backup: true,
        }));

        let err = dispatch_guarded(
            VerbArgs::Update(UpdateArgs {
                id: uid("grd-b", "lxc", "100"),
                action: "configure".into(),
                payload: None,
            }),
            true,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("pre-mutation backup"),
            "got: {err}"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![ACTION_BACKUP.to_string()],
            "mutation must NOT run after a failed backup"
        );
        assert!(deregister_provider("grd-b"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn guard_skips_backup_when_disabled_or_unguarded() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        register_provider(Arc::new(RecordingProvider {
            name: "grd-c".into(),
            calls: calls.clone(),
            fail_backup: false,
        }));

        // back_up = false → straight through, no backup.
        dispatch_guarded(
            VerbArgs::Update(UpdateArgs {
                id: uid("grd-c", "lxc", "100"),
                action: "configure".into(),
                payload: None,
            }),
            false,
        )
        .await
        .unwrap();
        // Unguarded action (`start`) → no backup even with back_up = true.
        dispatch_guarded(
            VerbArgs::Update(UpdateArgs {
                id: uid("grd-c", "lxc", "100"),
                action: "start".into(),
                payload: None,
            }),
            true,
        )
        .await
        .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["configure".to_string(), "start".to_string()],
            "neither call should trigger a backup"
        );
        assert!(deregister_provider("grd-c"));
    }
}
