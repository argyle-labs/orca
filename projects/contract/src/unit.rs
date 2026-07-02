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
}
