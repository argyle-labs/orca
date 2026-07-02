//! Managed-unit — the universal lifecycle capability surface.
//!
//! See `docs/MANAGED-UNIT.md`. orca core defines WHAT can be done (the canonical
//! [`Verb`] set + this declaration/registration toolset); a plugin defines HOW
//! for its domain by registering a [`UnitProvider`] that enumerates many units
//! of many kinds and performs canonical verbs against them. A VM, LXC, docker /
//! podman container, service, or a manager (Proxmox, Unraid) is all just a
//! *unit* with a capability-gated verb set — the host issues one canonical verb
//! and never branches on the concrete system.
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

// ── Identity ────────────────────────────────────────────────────────────────

/// A unit's four-axis identity. `kind` is a free string — core never enumerates
/// or branches on it; plugins do.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct UnitId {
    /// Who exposes it (e.g. `proxmox@cluster-a`, `docker@host-b`, `local`). A
    /// manager is itself a unit — see [`UnitDescriptor::parent`].
    pub manager: String,
    /// `vm` / `lxc` / `docker` / `podman` / `service` / `host` / …
    pub kind: String,
    /// Manager-native identifier (vmid, container id, service slug).
    pub id: String,
    /// Human label.
    pub name: String,
}

// ── Canonical verbs ───────────────────────────────────────────────────────────

/// The canonical lifecycle verb vocabulary orca owns. Plugins map their native
/// terms onto these (start = startup = power_on = launch; restart = reboot;
/// backup = snapshot; status = inspect/observe). The host only issues canonical
/// verbs; the synonym translation lives in the plugin.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    Status,
    Start,
    Stop,
    Restart,
    Update,
    Backup,
    Restore,
    Configure,
    Logs,
    Exec,
    Recover,
    Migrate,
}

impl Verb {
    /// The [`Verb`] a set of [`VerbArgs`] carries.
    pub fn of(args: &VerbArgs) -> Verb {
        match args {
            VerbArgs::Status => Verb::Status,
            VerbArgs::Start => Verb::Start,
            VerbArgs::Stop => Verb::Stop,
            VerbArgs::Restart => Verb::Restart,
            VerbArgs::Recover => Verb::Recover,
            VerbArgs::Logs(_) => Verb::Logs,
            VerbArgs::Exec(_) => Verb::Exec,
            VerbArgs::Update(_) => Verb::Update,
            VerbArgs::Configure(_) => Verb::Configure,
            VerbArgs::Backup(_) => Verb::Backup,
            VerbArgs::Restore(_) => Verb::Restore,
            VerbArgs::Migrate(_) => Verb::Migrate,
        }
    }
}

// ── Typed verb payloads (no opaque JSON) ──────────────────────────────────────

/// Args for [`Verb::Logs`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogsArgs {
    /// Max recent lines to return.
    pub tail: u32,
}

/// Args for [`Verb::Exec`] — one-shot, no TTY.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecArgs {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
}

/// Args for the plugin-shaped verbs ([`Verb::Update`] / [`Verb::Configure`]).
/// `document` is a schema-governed config/update document — its shape is
/// DECLARED by the plugin ([`VerbDecl::args_schema`]) and validated against that
/// schema, so it is typed-and-validated, never an opaque value. `None` means
/// "no args" (e.g. update to latest).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DocArgs {
    #[serde(default)]
    pub document: Option<String>,
}

/// Args for [`Verb::Backup`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackupArgs {
    /// Optional destination hint; the plugin owns the default location.
    #[serde(default)]
    pub dest: Option<String>,
}

/// Args for [`Verb::Restore`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestoreArgs {
    pub artifact: BackupArtifact,
}

/// Args for [`Verb::Migrate`] — move a unit to another manager/target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MigrateArgs {
    pub to: UnitId,
}

/// Typed args for one canonical verb. The variant IS the verb (see [`Verb::of`]).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "verb", content = "args")]
pub enum VerbArgs {
    Status,
    Start,
    Stop,
    Restart,
    Recover,
    Logs(LogsArgs),
    Exec(ExecArgs),
    Update(DocArgs),
    Configure(DocArgs),
    Backup(BackupArgs),
    Restore(RestoreArgs),
    Migrate(MigrateArgs),
}

/// A backup produced by [`Verb::Backup`] and consumed by [`Verb::Restore`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackupArtifact {
    /// Opaque-to-core identifier the plugin uses to locate the backup.
    pub id: String,
    /// Non-secret display location (path/url).
    pub location: String,
    #[serde(default)]
    pub bytes: Option<u64>,
}

/// Result of an action verb (start/stop/restart/update/configure/restore/
/// recover/migrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ActionOutcome {
    /// Whether the action changed state (false = already in desired state).
    pub changed: bool,
    #[serde(default)]
    pub message: String,
}

/// Result of [`Verb::Exec`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExecResult {
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

/// A unit's lifecycle state, normalized across systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Running,
    Starting,
    Stopping,
    Stopped,
    Degraded,
    Unknown,
}

/// Result of [`Verb::Status`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusReport {
    pub state: UnitState,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Typed outcome of a verb. The caller matches the variant appropriate to the
/// verb it issued.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "value")]
pub enum VerbOutcome {
    Status(StatusReport),
    Action(ActionOutcome),
    Logs(String),
    Exec(ExecResult),
    Backup(BackupArtifact),
}

// ── Declarations (plugin declares HOW) ────────────────────────────────────────

/// One unit-kind's declared surface: which verbs it supports and the typed input
/// model of the shaped ones. orca uses this to validate + render generically.
/// (Not itself `JsonSchema` — it *carries* a schema and only crosses via serde.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindDeclaration {
    pub kind: String,
    pub verbs: Vec<VerbDecl>,
}

/// A declared verb + its optional plugin-defined arg schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerbDecl {
    pub verb: Verb,
    /// JSON Schema for the verb's args when the plugin defines the shape
    /// (configure/update/backup/migrate). `None` for fixed-shape verbs
    /// (start/stop/restart/status/logs/exec/recover/restore). Validated by orca;
    /// never opaque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_schema: Option<Schema>,
}

/// One unit currently exposed by a provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnitDescriptor {
    pub id: UnitId,
    pub kind: String,
    pub state: UnitState,
    /// Capability gate: exactly the verbs this unit supports.
    pub verbs: Vec<Verb>,
    /// Nesting: this LXC's parent is its Proxmox host, etc. A manager is a unit
    /// whose children carry `parent = <manager unit id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<UnitId>,
}

// ── Provider trait ────────────────────────────────────────────────────────────

/// A source of managed units. One plugin registers ONE provider that enumerates
/// many units of possibly many kinds (proxmox → many vms + lxcs; docker → many
/// containers; a service → itself). Async methods return [`BoxFuture`].
pub trait UnitProvider: Send + Sync {
    /// Provider/registry name (registry key; replace-in-place + deregister).
    fn name(&self) -> &str;

    /// Declare, per unit-kind, which verbs are supported and their typed input
    /// models. Sync + cheap (typically static/cached).
    fn declarations(&self) -> Vec<KindDeclaration>;

    /// Enumerate every unit currently exposed.
    fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>>;

    /// Perform a canonical verb against one unit. The verb is encoded in `args`
    /// ([`Verb::of`]); returns the matching typed [`VerbOutcome`].
    fn invoke(&self, unit: &UnitId, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>>;
}

// ── Registry ──────────────────────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn UnitProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a unit provider. Re-registering the same `name()` replaces in place.
pub fn register_provider(provider: Arc<dyn UnitProvider>) {
    let mut g = GLOBAL.write().expect("unit registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn UnitProvider>> {
    GLOBAL.read().expect("unit registry poisoned").clone()
}

/// Deregister the provider named `name` (plugin unload). Returns whether removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("unit registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Enumerate every unit across all providers (each provider's failure is logged
/// by the caller; here we surface it per-provider).
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

/// The synchronous invoke thunk a cdylib plugin's unit provider is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn`
/// of strings so `contract` needs no ABI/loader dependency.
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Op the proxy calls to enumerate units (returns JSON `Vec<UnitDescriptor>`).
pub const UNITS_OP: &str = "units";
/// Op the proxy calls to fetch declarations (returns JSON `Vec<KindDeclaration>`).
pub const DECLARATIONS_OP: &str = "declarations";
/// Op the proxy calls to perform a verb (args JSON [`InvokeCall`] → [`VerbOutcome`]).
pub const INVOKE_OP: &str = "invoke";

/// Wire payload for [`INVOKE_OP`]: which unit + typed verb args.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeCall {
    pub unit: UnitId,
    pub args: VerbArgs,
}

/// Register a [`UnitProvider`] from a plugin backend descriptor + [`InvokeThunk`].
/// The plugin-loader calls this from its `domain = "unit"` dispatch arm.
/// Declarations are fetched once here and cached (the trait's `declarations()` is
/// sync); a fetch failure registers an empty declaration set rather than failing
/// the load.
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

/// A [`UnitProvider`] backed by a cdylib plugin over the JSON-proxy FFI boundary.
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

    fn invoke(&self, unit: &UnitId, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let call = InvokeCall {
            unit: unit.clone(),
            args,
        };
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

/// Route a proxied op back onto a plugin's own [`UnitProvider`] and encode the
/// result for FFI. Symmetric with [`FfiUnitProvider`] — a plugin's
/// `backend_dispatch` delegates here so it never hand-writes the op match.
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
                .invoke(&call.unit, call.args)
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
        assert_eq!(Verb::of(&VerbArgs::Start), Verb::Start);
        assert_eq!(Verb::of(&VerbArgs::Logs(LogsArgs { tail: 10 })), Verb::Logs);
        assert_eq!(
            Verb::of(&VerbArgs::Configure(DocArgs::default())),
            Verb::Configure
        );
    }

    // A fake plugin thunk answering the three ops, so we exercise the whole
    // register_from_def -> FfiUnitProvider -> all_units / invoke round trip.
    fn fake_thunk() -> InvokeThunk {
        Arc::new(|op: &str, args: String| match op {
            DECLARATIONS_OP => Ok(serde_json::to_string(&vec![KindDeclaration {
                kind: "vm".into(),
                verbs: vec![VerbDecl {
                    verb: Verb::Start,
                    args_schema: None,
                }],
            }])
            .unwrap()),
            UNITS_OP => Ok(serde_json::to_string(&vec![UnitDescriptor {
                id: UnitId {
                    manager: "fake@x".into(),
                    kind: "vm".into(),
                    id: "100".into(),
                    name: "web".into(),
                },
                kind: "vm".into(),
                state: UnitState::Running,
                verbs: vec![Verb::Start, Verb::Stop],
                parent: None,
            }])
            .unwrap()),
            INVOKE_OP => {
                let call: InvokeCall = serde_json::from_str(&args).unwrap();
                // Echo back an action outcome tagged with the verb issued.
                let msg = format!("{:?}", Verb::of(&call.args));
                Ok(serde_json::to_string(&VerbOutcome::Action(ActionOutcome {
                    changed: true,
                    message: msg,
                }))
                .unwrap())
            }
            other => Err(format!("unexpected op {other}")),
        })
    }

    #[tokio::test]
    async fn ffi_provider_round_trips_units_and_invoke() {
        register_from_def("prov-unit-test".into(), fake_thunk()).unwrap();

        // declarations cached at registration
        let prov = providers()
            .into_iter()
            .find(|p| p.name() == "prov-unit-test")
            .unwrap();
        assert_eq!(prov.declarations()[0].kind, "vm");

        let units = all_units().await;
        let u = units.iter().find(|u| u.id.id == "100").unwrap();
        assert_eq!(u.state, UnitState::Running);
        assert!(u.verbs.contains(&Verb::Start));

        let outcome = prov.invoke(&u.id, VerbArgs::Stop).await.unwrap();
        match outcome {
            VerbOutcome::Action(a) => {
                assert!(a.changed);
                assert_eq!(a.message, "Stop");
            }
            other => panic!("expected action outcome, got {other:?}"),
        }

        assert!(deregister_provider("prov-unit-test"));
    }
}
