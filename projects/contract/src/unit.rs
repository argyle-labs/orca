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

// ── Five canonical verbs ──────────────────────────────────────────────────────

/// The complete canonical verb vocabulary. Five verbs cover every domain:
///
/// - [`List`]   — GET collection with query params (search, filter, log tail, …)
/// - [`Detail`] — GET one item (unit state, metadata, logs with query params)
/// - [`Create`] — POST something new (provision VM, add media, take backup, exec)
/// - [`Update`] — PATCH state (start/stop/restart/migrate/restore/configure/bump)
/// - [`Delete`] — DELETE (destroy, remove)
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

/// Typed args for one canonical verb. The variant IS the verb ([`Verb::of`]).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "verb", content = "args")]
pub enum VerbArgs {
    List(ListArgs),
    Detail(DetailArgs),
    Create(CreateArgs),
    Update(UpdateArgs),
    Delete(DeleteArgs),
}

// ── Typed outcomes ────────────────────────────────────────────────────────────

/// A single item returned by [`Verb::Detail`] or a [`Verb::Create`]/[`Verb::Update`]
/// that produces one resource. `payload` is schema-validated JSON from the plugin.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ItemOutcome {
    pub id: UnitId,
    /// Schema-validated JSON payload; shape declared by the plugin's response schema.
    pub payload: String,
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
}

/// One kind's declared surface. A provider returns many of these — Sonarr
/// returns `[tv_show, season, episode]`; proxmox returns `[vm, lxc, host]`.
/// No kind is owned by a plugin; any provider may declare any kind string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindDeclaration {
    /// Free kind string (`vm`, `lxc`, `tv_show`, `movie`, `service`, …).
    pub kind: String,
    pub verbs: Vec<VerbDecl>,
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
    providers().into_iter().find(|p| {
        let n = p.name();
        id.manager == n || id.manager.starts_with(&format!("{n}@"))
    })
}

/// Route a verb to the right provider(s) and return the merged outcome.
///
/// - [`Verb::List`] fans out to every provider (or only those declaring
///   `query.kind` when set) and merges the items.
/// - [`Verb::Detail`] / [`Verb::Update`] / [`Verb::Delete`] route to the single
///   provider that [`owner_of`] the target id.
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
            let mut saw_total = false;
            for p in targets {
                match p.invoke(VerbArgs::List(l.clone())).await {
                    Ok(VerbOutcome::Items(items)) => {
                        if let Some(t) = items.total {
                            saw_total = true;
                            merged.total = Some(merged.total.unwrap_or(0) + t);
                        }
                        merged.items.extend(items.items);
                    }
                    Ok(other) => {
                        return Err(anyhow::anyhow!(
                            "provider '{}' returned non-list outcome for List: {other:?}",
                            p.name()
                        ));
                    }
                    // A single provider failing a broad list must not sink the
                    // whole query — skip it and keep merging the rest.
                    Err(_) => continue,
                }
            }
            if !saw_total {
                merged.total = Some(merged.items.len() as u64);
            }
            Ok(VerbOutcome::Items(merged))
        }
        VerbArgs::Detail(d) => route_targeted(&d.id.clone(), VerbArgs::Detail(d)).await,
        VerbArgs::Update(u) => route_targeted(&u.id.clone(), VerbArgs::Update(u)).await,
        VerbArgs::Delete(d) => route_targeted(&d.id.clone(), VerbArgs::Delete(d)).await,
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

// ── FFI bridge ────────────────────────────────────────────────────────────────

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

struct FfiUnitProvider {
    name: String,
    invoke: InvokeThunk,
    declarations: Vec<KindDeclaration>,
}

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

    fn fake_thunk() -> InvokeThunk {
        Arc::new(|op: &str, args: String| match op {
            DECLARATIONS_OP => Ok(serde_json::to_string(&vec![KindDeclaration {
                kind: "vm".into(),
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
                            .map(|id| ItemOutcome {
                                id: id.clone(),
                                payload: "{}".into(),
                            })
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

    #[tokio::test]
    async fn dispatch_targeted_unknown_owner_errors() {
        let err = dispatch(VerbArgs::Delete(DeleteArgs {
            id: uid("ghost@x", "vm", "999"),
        }))
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no provider owns"), "got: {err}");
    }

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
}
