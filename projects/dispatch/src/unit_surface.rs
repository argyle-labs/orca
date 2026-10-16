//! Runtime unit surface — projects [`contract::unit::catalog()`] onto MCP, HTTP
//! (OpenAPI), and CLI.
//!
//! The `#[orca_tool]` inventory is **static** (linker-time): each tool's name,
//! schema, and handler are fixed at build. The unit surface is the opposite —
//! kinds and actions are only known at runtime from *loaded plugins*. So this
//! module is the dynamic mirror of [`crate::registry`]: it walks the live
//! provider catalog and emits the same three projections every `#[orca_tool]`
//! gets, but rebuilt on demand so adding a plugin self-enriches every surface
//! with zero code change.
//!
//! One intermediate ([`UnitOp`]), three projections:
//! - [`unit_mcp_defs`]  → merged into `tools/list`
//! - [`inject_unit_openapi`] data → OpenAPI paths (rendered in [`crate::openapi`])
//! - [`unit_cli_commands_from`] → top-level per-kind clap subcommands (`orca vm …`)
//!
//! And one dispatcher ([`unit_dispatch`]) that routes an incoming call back
//! through [`contract::unit::dispatch`] / [`contract::unit::dispatch_to`].
//!
//! Naming: `unit` is an internal abstraction consumers never see. Every op is
//! named `<kind>.<verb-or-action>` — `container.list`, `container.exec`,
//! `vm.start`. The dotted form is the canonical tool NAME across REST/MCP; the
//! CLI exposes each kind as a TOP-LEVEL command: `orca container list`,
//! `orca vm start`, `orca lxc list`.
//!
//! The wire envelope is uniform and collision-free — never flattened:
//! - `id`      — the target [`contract::unit::UnitId`] (detail/update/delete)
//! - `query`   — [`contract::unit::QueryArgs`] (list/detail)
//! - `payload` — the plugin-declared, typed action payload (create/update)
//! - `provider`— provider selector for create when a kind has more than one
//!
//! Every field's schema is real and plugin-declared; the transport being JSON
//! is behind that typed schema, exactly as the `#[orca_tool]` layer already is.

// The tool-schema layer is inherently dynamic JSON: we assemble plugin-declared
// `schemars::Schema`s (themselves fully typed at their source) into MCP/OpenAPI/
// CLI wire formats. This mirrors `registry.rs` and `openapi.rs`, which carry the
// same allow for the same reason.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use serde_json::{Map, Value, json};

use contract::unit::{
    self, CreateArgs, DeleteArgs, DetailArgs, ListArgs, QueryArgs, UnitId, UpdateArgs, UpsertArgs,
    Verb, VerbArgs,
};

/// The canonical intermediate: one operator-facing operation, with the typed
/// schemas the three surfaces project from. Rebuilt from the live catalog.
/// Serializable so the CLI (a separate process) can fetch the daemon's live
/// catalog and build its command tree + `--help` from what's actually loaded.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UnitOp {
    /// Canonical dotted name: `<kind>.<verb-or-action>` (e.g. `vm.list`,
    /// `container.exec`). No `unit.` prefix — that abstraction is internal.
    pub name: String,
    /// The kind this op targets (`container`, `vm`, …).
    pub kind: String,
    /// The canonical verb this op maps to.
    pub verb: Verb,
    /// The create/update action (`exec`, `start`, …); `None` for list/detail/delete.
    pub action: Option<String>,
    /// Human description for tools/list, OpenAPI summary, and CLI help.
    pub description: String,
    /// Providers exposing this (kind, verb, action). >1 ⇒ create needs `provider`.
    pub providers: Vec<String>,
    /// Whether this op mutates state (everything but list/detail).
    pub mutates: bool,
    /// Typed JSON Schema for the wire input envelope.
    pub input_schema: Value,
    /// Typed JSON Schema for the response.
    pub output_schema: Value,
}

// ── Routing spec (cheap; no schema build) ───────────────────────────────────────

/// The minimum needed to route a call — no schema construction. Used by
/// [`unit_dispatch`] / [`unit_owns`] on the hot path.
struct OpSpec {
    name: String,
    kind: String,
    verb: Verb,
    action: Option<String>,
    providers: Vec<String>,
}

/// Walk the live catalog and enumerate every routable op, merging providers
/// that expose the same (kind, verb, action).
fn op_specs() -> Vec<OpSpec> {
    let mut specs: Vec<OpSpec> = Vec::new();
    let mut push =
        |name: String, kind: &str, verb: Verb, action: Option<String>, provider: &str| {
            if shadowed(&name) {
                return;
            }
            if let Some(existing) = specs.iter_mut().find(|s| s.name == name) {
                if !existing.providers.iter().any(|p| p == provider) {
                    existing.providers.push(provider.to_string());
                }
            } else {
                specs.push(OpSpec {
                    name,
                    kind: kind.to_string(),
                    verb,
                    action,
                    providers: vec![provider.to_string()],
                });
            }
        };

    for entry in unit::catalog() {
        for vd in &entry.verbs {
            match vd.verb {
                Verb::List => push(
                    format!("{}.list", entry.kind),
                    &entry.kind,
                    Verb::List,
                    None,
                    &entry.provider,
                ),
                Verb::Detail => push(
                    format!("{}.detail", entry.kind),
                    &entry.kind,
                    Verb::Detail,
                    None,
                    &entry.provider,
                ),
                Verb::Delete => push(
                    format!("{}.delete", entry.kind),
                    &entry.kind,
                    Verb::Delete,
                    None,
                    &entry.provider,
                ),
                Verb::Create | Verb::Update | Verb::Upsert => {
                    for act in &vd.actions {
                        push(
                            format!("{}.{}", entry.kind, act.action),
                            &entry.kind,
                            vd.verb,
                            Some(act.action.clone()),
                            &entry.provider,
                        );
                    }
                }
            }
        }
    }
    specs
}

fn shadowed(name: &str) -> bool {
    crate::registry::find(name).is_some()
}

fn resolve(name: &str) -> Option<OpSpec> {
    op_specs().into_iter().find(|s| s.name == name)
}

/// True iff a loaded unit provider exposes an op with this name. Names are
/// `<kind>.<verb>` (no `unit.` prefix — that's internal), so ownership is
/// decided by live-catalog membership, not a name prefix.
pub fn unit_owns(name: &str) -> bool {
    op_specs().iter().any(|s| s.name == name)
}

/// `(required_role, data_mutation)` for a live unit op, or `None` if `name` is
/// not one. Providers declare no per-action roles, so this follows the
/// `#[orca_tool]` default: reads are `any`, every create/update/upsert/delete
/// is an `admin` data mutation.
pub fn unit_policy(name: &str) -> Option<(&'static str, bool)> {
    let spec = resolve(name)?;
    Some(match spec.verb {
        Verb::List | Verb::Detail => ("any", false),
        Verb::Create | Verb::Update | Verb::Upsert | Verb::Delete => ("admin", true),
    })
}

pub fn unit_dry_run_ok(name: &str) -> bool {
    resolve(name).is_some_and(|s| s.verb == Verb::Delete)
}

// ── Full ops (with schemas) ─────────────────────────────────────────────────────

/// The full operator-facing op list, with typed schemas. Powers `tools/list`,
/// the OpenAPI paths, and the CLI. Rebuilt from the live catalog each call.
pub fn unit_ops() -> Vec<UnitOp> {
    // Re-walk the catalog so we keep each op's declared payload/response schemas
    // and query schema; op_specs() alone drops them.
    let mut ops: Vec<UnitOp> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in unit::catalog() {
        let kind = &entry.kind;
        for vd in &entry.verbs {
            match vd.verb {
                Verb::List => {
                    let name = format!("{kind}.list");
                    if seen.insert(name.clone()) {
                        ops.push(UnitOp {
                            name,
                            kind: kind.clone(),
                            verb: Verb::List,
                            action: None,
                            description: format!("List {kind} units (search/filter via query)"),
                            providers: providers_for(kind, Verb::List, None),
                            mutates: false,
                            input_schema: list_input_schema(vd.query_schema.as_ref()),
                            output_schema: schema_value::<unit::ItemsOutcome>(),
                        });
                    }
                }
                Verb::Detail => {
                    let name = format!("{kind}.detail");
                    if seen.insert(name.clone()) {
                        ops.push(UnitOp {
                            name,
                            kind: kind.clone(),
                            verb: Verb::Detail,
                            action: None,
                            description: format!("Inspect one {kind} unit"),
                            providers: providers_for(kind, Verb::Detail, None),
                            mutates: false,
                            input_schema: detail_input_schema(vd.query_schema.as_ref()),
                            output_schema: schema_value::<unit::ItemOutcome>(),
                        });
                    }
                }
                Verb::Delete => {
                    let name = format!("{kind}.delete");
                    if seen.insert(name.clone()) {
                        ops.push(UnitOp {
                            name,
                            kind: kind.clone(),
                            verb: Verb::Delete,
                            action: None,
                            description: format!("Delete a {kind} unit"),
                            providers: providers_for(kind, Verb::Delete, None),
                            mutates: true,
                            input_schema: delete_input_schema(),
                            output_schema: schema_value::<unit::ActionOutcome>(),
                        });
                    }
                }
                Verb::Create | Verb::Update | Verb::Upsert => {
                    for act in &vd.actions {
                        let name = format!("{kind}.{}", act.action);
                        if !seen.insert(name.clone()) {
                            continue;
                        }
                        let providers = providers_for(kind, vd.verb, Some(&act.action));
                        let payload = act.payload_schema.as_ref().map(schema_to_value);
                        let response = act.response_schema.as_ref().map(schema_to_value);
                        let (input_schema, output_schema, desc) = match vd.verb {
                            Verb::Create => (
                                create_input_schema(payload.as_ref(), providers.len() > 1),
                                response.unwrap_or_else(schema_value::<unit::VerbOutcome>),
                                format!("Create ({}) on a {kind}", act.action),
                            ),
                            Verb::Upsert => (
                                update_input_schema(payload.as_ref()),
                                response.unwrap_or_else(schema_value::<unit::VerbOutcome>),
                                format!("Upsert ({}) a {kind} unit by key", act.action),
                            ),
                            _ => (
                                update_input_schema(payload.as_ref()),
                                response.unwrap_or_else(schema_value::<unit::ActionOutcome>),
                                format!("Update ({}) a {kind} unit", act.action),
                            ),
                        };
                        ops.push(UnitOp {
                            name,
                            kind: kind.clone(),
                            verb: vd.verb,
                            action: Some(act.action.clone()),
                            description: desc,
                            providers,
                            mutates: true,
                            input_schema,
                            output_schema,
                        });
                    }
                }
            }
        }
    }
    ops.retain(|op| !shadowed(&op.name));
    ops
}

fn providers_for(kind: &str, verb: Verb, action: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for entry in unit::catalog() {
        if entry.kind != kind {
            continue;
        }
        for vd in &entry.verbs {
            if vd.verb != verb {
                continue;
            }
            let matches = match action {
                Some(a) => vd.actions.iter().any(|ad| ad.action == a),
                None => true,
            };
            if matches && !out.contains(&entry.provider) {
                out.push(entry.provider.clone());
            }
        }
    }
    out
}

// ── Schema construction ─────────────────────────────────────────────────────────

fn schema_value<T: schemars::JsonSchema>() -> Value {
    strip_meta(schemars::schema_for!(T).into())
}

fn schema_to_value(s: &schemars::Schema) -> Value {
    strip_meta(serde_json::to_value(s).unwrap_or_else(|_| json!({ "type": "object" })))
}

fn strip_meta(mut v: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    v
}

fn target_schema(defs: &mut Map<String, Value>) -> Value {
    json!({
        "anyOf": [
            embed(schema_value::<UnitId>(), defs),
            {
                "type": "string",
                "description": "Unit name or manager-native id; resolved to the manager that owns it on the target system"
            }
        ]
    })
}

fn delete_input_schema() -> Value {
    let mut defs = Map::new();
    let id = target_schema(&mut defs);
    finish(
        json!({
            "type": "object",
            "properties": {
                "id": id,
                (contract::plan::DRY_RUN_FIELD): {
                    "type": "boolean",
                    "default": false,
                    "description": "Describe the delete without applying it: resolves the owning manager and changes nothing."
                }
            },
            "required": ["id"],
        }),
        defs,
    )
}

/// `{ query?: QueryArgs (+ plugin query fields) }` — list.
fn list_input_schema(extra: Option<&schemars::Schema>) -> Value {
    let mut defs = Map::new();
    let query = query_schema(extra, &mut defs);
    finish(
        json!({
            "type": "object",
            "properties": { "query": query },
        }),
        defs,
    )
}

/// `{ id: UnitId, query?: QueryArgs }` — detail.
fn detail_input_schema(extra: Option<&schemars::Schema>) -> Value {
    let mut defs = Map::new();
    let id = target_schema(&mut defs);
    let query = query_schema(extra, &mut defs);
    finish(
        json!({
            "type": "object",
            "properties": { "id": id, "query": query },
            "required": ["id"],
        }),
        defs,
    )
}

/// `{ id: UnitId, payload?: <declared> }` — update actions.
fn update_input_schema(payload: Option<&Value>) -> Value {
    let mut defs = Map::new();
    let id = target_schema(&mut defs);
    let mut props = Map::new();
    props.insert("id".into(), id);
    if let Some(p) = payload {
        props.insert("payload".into(), embed(p.clone(), &mut defs));
    }
    finish(
        json!({ "type": "object", "properties": props, "required": ["id"] }),
        defs,
    )
}

/// `{ payload?: <declared>, provider?: string }` — create actions. `provider`
/// is required only when more than one provider declares the kind.
fn create_input_schema(payload: Option<&Value>, multi_provider: bool) -> Value {
    let mut defs = Map::new();
    let mut props = Map::new();
    if let Some(p) = payload {
        props.insert("payload".into(), embed(p.clone(), &mut defs));
    }
    props.insert(
        "provider".into(),
        json!({
            "type": "string",
            "description": if multi_provider {
                "Which provider to create on (required — multiple providers expose this kind)"
            } else {
                "Which provider to create on (optional — one provider exposes this kind)"
            }
        }),
    );
    let required = if multi_provider {
        json!(["provider"])
    } else {
        json!([])
    };
    finish(
        json!({ "type": "object", "properties": props, "required": required }),
        defs,
    )
}

fn query_schema(extra: Option<&schemars::Schema>, defs: &mut Map<String, Value>) -> Value {
    let base = schema_value::<QueryArgs>();
    match extra {
        Some(s) => {
            // Merge plugin-declared extra query fields into QueryArgs' properties.
            let mut base = embed(base, defs);
            let extra_v = embed(schema_to_value(s), defs);
            if let (Some(bo), Some(eo)) = (base.as_object_mut(), extra_v.as_object())
                && let (Some(Value::Object(bp)), Some(Value::Object(ep))) =
                    (bo.get_mut("properties"), eo.get("properties"))
            {
                for (k, v) in ep {
                    bp.insert(k.clone(), v.clone());
                }
            }
            base
        }
        None => embed(base, defs),
    }
}

/// Lift a sub-schema's own `$defs` up into the shared `defs` map (so `$ref`s
/// that point at `#/$defs/X` resolve at the document root), returning the
/// sub-schema with its `$defs` removed.
fn embed(mut schema: Value, defs: &mut Map<String, Value>) -> Value {
    if let Some(obj) = schema.as_object_mut()
        && let Some(Value::Object(inner)) = obj.remove("$defs")
    {
        for (k, v) in inner {
            defs.entry(k).or_insert(v);
        }
    }
    schema
}

/// Attach the collected `$defs` (if any) to the root schema object.
fn finish(mut root: Value, defs: Map<String, Value>) -> Value {
    if !defs.is_empty()
        && let Some(o) = root.as_object_mut()
    {
        o.insert("$defs".into(), Value::Object(defs));
    }
    root
}

// ── MCP projection ──────────────────────────────────────────────────────────────

/// Unit ops as MCP `tools/list` entries, merged by [`crate::registry`].
pub fn unit_mcp_defs() -> Vec<Value> {
    unit_ops()
        .into_iter()
        .map(|op| {
            json!({
                "name": op.name,
                "description": op.description,
                "inputSchema": crate::registry::advertise_system(op.input_schema),
            })
        })
        .collect()
}

// ── CLI projection ──────────────────────────────────────────────────────────────

/// The live catalog as JSON, for the daemon's catalog endpoint. The CLI fetches
/// this to build its command tree against what's actually loaded.
pub fn unit_catalog_json() -> Value {
    serde_json::to_value(unit_ops()).unwrap_or_else(|_| json!([]))
}

/// Distinct kinds currently exposed by loaded providers — the set of top-level
/// CLI commands the unit surface owns (`vm`, `lxc`, `container`, …).
///
/// This is the PRE-collision-filter list: it reports every kind the providers
/// claim, including any whose name is already taken by a static command. Do NOT
/// use it to decide dispatch routing — a caller that does will route a colliding
/// name to the unit surface even though "static wins" kept it unregistered.
/// `is_dynamic_domain` wants exactly this unfiltered view and documents why.
pub fn unit_kinds_from(ops: &[UnitOp]) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for op in ops {
        if !kinds.contains(&op.kind) {
            kinds.push(op.kind.clone());
        }
    }
    kinds
}

/// Build the top-level per-kind clap commands from the local catalog. See
/// [`unit_cli_commands_from`] for the form the CLI uses with a catalog fetched
/// from the daemon.
pub fn unit_cli_commands() -> Vec<clap::Command> {
    unit_cli_commands_from(unit_ops())
}

/// Build one **top-level** clap command per kind (`orca vm …`, `orca lxc …`,
/// `orca container …`) from an explicit op list. `unit` never appears — each
/// kind is a first-class command. Each verb/action leaf accepts `--json '{…}'`
/// or `key=value` pairs; its `--help` shows the live description + typed input
/// schema, reflecting exactly what the currently-loaded plugins expose.
/// Splice the unit commands into `root` and return the `<kind>.<leaf>` names
/// they route. A kind that is already a static command keeps its static leaves
/// and gains only the unit leaves it lacks, so both share one `orca <kind>`.
pub fn register_unit_commands(
    mut root: clap::Command,
    ops: Vec<UnitOp>,
) -> (clap::Command, Vec<String>) {
    let mut routed = Vec::new();
    for cmd in unit_cli_commands_from(ops) {
        let kind = cmd.get_name().to_string();
        let taken: Option<std::collections::HashSet<String>> =
            root.find_subcommand(&kind).map(|c| {
                c.get_subcommands()
                    .map(|l| l.get_name().to_string())
                    .collect()
            });
        let leaves: Vec<clap::Command> = cmd
            .get_subcommands()
            .filter(|l| taken.as_ref().is_none_or(|t| !t.contains(l.get_name())))
            .cloned()
            .collect();
        routed.extend(leaves.iter().map(|l| format!("{kind}.{}", l.get_name())));
        root = match taken {
            None => root.subcommand(cmd),
            Some(_) => root.mut_subcommand(kind, |c| c.subcommands(leaves)),
        };
    }
    (root, routed)
}

pub fn unit_cli_commands_from(ops: Vec<UnitOp>) -> Vec<clap::Command> {
    use std::collections::BTreeMap;

    // clap interns command names as `&'static str`. The CLI tree is built once
    // per short-lived `orca` invocation, so leaking the handful of dynamic
    // kind/op names is bounded and the accepted pattern for runtime clap trees.
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    // group ops by kind
    let mut by_kind: BTreeMap<String, Vec<UnitOp>> = BTreeMap::new();
    for op in ops {
        by_kind.entry(op.kind.clone()).or_default().push(op);
    }

    let mut commands = Vec::new();
    for (kind, mut ops) in by_kind {
        ops.sort_by(|a, b| a.name.cmp(&b.name));
        let mut kind_cmd = clap::Command::new(leak(kind.clone()))
            .about(format!("Manage {kind} units (live, plugin-driven)"))
            .subcommand_required(true)
            .arg_required_else_help(true);
        for op in ops {
            // Verb/action leaf name = last dotted segment (`vm.start` → `start`).
            let leaf = op.name.rsplit('.').next().unwrap_or(&op.name).to_string();
            let help = format!(
                "{}\n\nInput schema:\n{}",
                op.description,
                serde_json::to_string_pretty(&op.input_schema).unwrap_or_default()
            );
            let leaf_cmd = clap::Command::new(leak(leaf))
                .about(op.description.clone())
                .long_about(help)
                .arg(
                    clap::Arg::new("json")
                        .long("json")
                        .value_name("JSON")
                        .help("Args as a JSON object matching the input schema"),
                )
                .arg(
                    clap::Arg::new("pairs")
                        .value_name("KEY=VALUE")
                        .num_args(0..)
                        .help("Args as key=value pairs (values parsed as JSON, else string)"),
                )
                .arg(crate::cli::system_id_flag());
            kind_cmd = kind_cmd.subcommand(leaf_cmd);
        }
        commands.push(kind_cmd);
    }
    commands
}

// ── Dispatch ────────────────────────────────────────────────────────────────────

/// Route a `unit.<kind>.<op>` call back through the contract-side dispatcher.
/// Returns `None` when `name` isn't a unit op (so the caller falls through).
/// `caller` is the request's verified identity, handed to the provider on
/// create/update/upsert/delete.
pub async fn unit_dispatch(
    name: &str,
    args: &Value,
    caller: Option<&contract::CallerIdentity>,
) -> Option<Result<Value>> {
    let spec = resolve(name)?;
    Some(run(spec, args, caller.cloned()).await)
}

async fn run(
    spec: OpSpec,
    args: &Value,
    caller: Option<contract::CallerIdentity>,
) -> Result<Value> {
    let outcome = match spec.verb {
        Verb::List => {
            let mut query: QueryArgs = args
                .get("query")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid query: {e}"))?
                .unwrap_or_default();
            // Scope the broad list to this op's kind.
            query.kind.get_or_insert_with(|| spec.kind.clone());
            unit::dispatch(VerbArgs::List(ListArgs { query })).await?
        }
        Verb::Detail => {
            let id = target_id(&spec, args).await?.id;
            let query: QueryArgs = args
                .get("query")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid query: {e}"))?
                .unwrap_or_default();
            unit::dispatch(VerbArgs::Detail(DetailArgs { id, query })).await?
        }
        Verb::Delete => {
            let target = target_id(&spec, args).await?;
            if crate::erased::dry_run_requested(args)? {
                return delete_plan(&spec.kind, target).await;
            }
            let id = target.id;
            unit::dispatch(VerbArgs::Delete(DeleteArgs { id, caller })).await?
        }
        Verb::Update => {
            let id = target_id(&spec, args).await?.id;
            let payload = args.get("payload").map(|v| v.to_string());
            let action = spec
                .action
                .clone()
                .ok_or_else(|| anyhow::anyhow!("update op missing action"))?;
            unit::dispatch(VerbArgs::Update(UpdateArgs {
                id,
                action,
                payload,
                caller,
            }))
            .await?
        }
        Verb::Upsert => {
            let id = target_id(&spec, args).await?.id;
            let payload = args.get("payload").map(|v| v.to_string());
            let action = spec
                .action
                .clone()
                .ok_or_else(|| anyhow::anyhow!("upsert op missing action"))?;
            unit::dispatch(VerbArgs::Upsert(UpsertArgs {
                id,
                action,
                payload,
                caller,
            }))
            .await?
        }
        Verb::Create => {
            let payload = args.get("payload").map(|v| v.to_string());
            let action = spec
                .action
                .clone()
                .ok_or_else(|| anyhow::anyhow!("create op missing action"))?;
            let call = CreateArgs {
                action,
                payload,
                caller,
            };
            // Pick the provider: explicit `provider`, else the sole declarer.
            let provider = args
                .get("provider")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    if spec.providers.len() == 1 {
                        spec.providers.first().cloned()
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "'{}' is exposed by multiple providers {:?}; set \"provider\"",
                        spec.name,
                        spec.providers
                    )
                })?;
            unit::dispatch_to(&provider, VerbArgs::Create(call)).await?
        }
    };
    // Unwrap the VerbOutcome envelope to its inner value so the response matches
    // the op's declared output schema (ItemsOutcome / ItemOutcome / ActionOutcome)
    // rather than the internal `{outcome, value}` tagged form.
    let value = match outcome {
        unit::VerbOutcome::Items(i) => serde_json::to_value(i),
        unit::VerbOutcome::Item(i) => serde_json::to_value(i),
        unit::VerbOutcome::Action(a) => serde_json::to_value(a),
    };
    value.map_err(|e| anyhow::anyhow!("encode outcome: {e}"))
}

struct Target {
    id: UnitId,
    listed: bool,
}

async fn target_id(spec: &OpSpec, args: &Value) -> Result<Target> {
    match args.get("id").and_then(Value::as_str).map(str::trim) {
        Some(sel) if !sel.is_empty() => Ok(Target {
            id: resolve_unit(&spec.kind, sel, spec.verb != Verb::Detail).await?,
            listed: true,
        }),
        _ => Ok(Target {
            id: parse_id(&spec.name, args)?,
            listed: false,
        }),
    }
}

fn resolve_timeout() -> std::time::Duration {
    static WAIT: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *WAIT.get_or_init(|| crate::registry::secs_from_env("ORCA_UNIT_RESOLVE_TIMEOUT_SECS", 15))
}

#[cfg(feature = "in-process")]
async fn bounded<T>(fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let wait = resolve_timeout();
    tokio::time::timeout(wait, fut)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {wait:?}"))?
}

#[cfg(not(feature = "in-process"))]
async fn bounded<T>(fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let _ = resolve_timeout;
    fut.await
}

async fn listed(provider: &dyn unit::UnitProvider, kind: &str) -> Result<Vec<UnitId>> {
    let mut ids: Vec<UnitId> = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let query = QueryArgs {
            kind: Some(kind.to_string()),
            offset: (offset > 0).then_some(offset),
            ..Default::default()
        };
        let page = match provider.invoke(VerbArgs::List(ListArgs { query })).await? {
            unit::VerbOutcome::Items(items) => items,
            _ => anyhow::bail!("{} returned a non-list outcome", provider.name()),
        };
        let n = page.items.len();
        for id in page.items.into_iter().map(|i| i.id) {
            if id.kind == kind && id.manager_base() == provider.name() && !ids.contains(&id) {
                ids.push(id);
            }
        }
        offset = offset.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        match page.total {
            Some(total) if n > 0 && u64::from(offset) < total => {}
            _ => return Ok(ids),
        }
    }
}

pub async fn resolve_unit(kind: &str, sel: &str, strict: bool) -> Result<UnitId> {
    let providers = unit::providers_for_kind(kind);
    let answers = futures::future::join_all(
        providers
            .iter()
            .map(|p| async move { (p.name(), bounded(listed(p.as_ref(), kind)).await) }),
    )
    .await;
    let mut found: Vec<UnitId> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for (name, answer) in answers {
        match answer {
            Ok(ids) => found.extend(ids.into_iter().filter(|id| id.id == sel || id.name == sel)),
            Err(e) => {
                tracing::warn!(provider = %name, error = %format!("{e:#}"), "unit resolve: list failed");
                failed.push(name);
            }
        }
    }
    let checked: Vec<&str> = providers.iter().map(|p| p.name()).collect();
    if strict && !failed.is_empty() {
        return Err(contract::OrcaError::unavailable(format!(
            "cannot resolve {kind} '{sel}': managers unreachable: {failed:?}; pass the full id"
        ))
        .into());
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 if failed.is_empty() => Err(contract::OrcaError::not_found(format!(
            "no {kind} '{sel}' on this system; managers checked: {checked:?}"
        ))
        .into()),
        0 => Err(contract::OrcaError::not_found(format!(
            "no {kind} '{sel}' on this system; managers checked: {checked:?}; unreachable: {failed:?}"
        ))
        .into()),
        _ => {
            let candidates: Vec<String> = found
                .iter()
                .map(|id| format!("{}/{} ({})", id.manager, id.id, id.name))
                .collect();
            Err(contract::OrcaError::invalid(format!(
                "{kind} '{sel}' is ambiguous across managers: {candidates:?}; pass the full id"
            ))
            .with_code("args.invalid")
            .into())
        }
    }
}

async fn delete_plan(kind: &str, target: Target) -> Result<Value> {
    let id = target.id;
    let owner = unit::owner_of(&id).ok_or_else(|| {
        anyhow::anyhow!(
            "no provider owns unit '{}' (manager '{}')",
            id.id,
            id.manager
        )
    })?;
    if !target.listed {
        let ids = bounded(listed(owner.as_ref(), kind)).await.map_err(|e| {
            tracing::warn!(provider = %owner.name(), error = %format!("{e:#}"), "unit delete dry run: list failed");
            contract::OrcaError::unavailable(format!("manager '{}' unreachable", id.manager))
        })?;
        if !ids.contains(&id) {
            return Err(contract::OrcaError::not_found(format!(
                "no {kind} '{}' on manager '{}'",
                id.id, id.manager
            ))
            .into());
        }
    }
    serde_json::to_value(unit::ActionOutcome {
        changed: false,
        message: format!(
            "dry run: would delete {kind} '{}' ({}) on {}",
            id.name, id.id, id.manager
        ),
    })
    .map_err(|e| anyhow::anyhow!("encode outcome: {e}"))
}

fn parse_id(tool: &str, args: &Value) -> Result<UnitId> {
    #[derive(serde::Deserialize)]
    struct IdArg {
        id: UnitId,
    }
    crate::erased::parse_named_args::<IdArg>(tool, args.clone()).map(|a| a.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::BoxFuture;
    use contract::unit::{
        ActionDecl, ActionOutcome, ItemOutcome, ItemsOutcome, KindDeclaration, UnitDescriptor,
        UnitProvider, VerbDecl, VerbOutcome, register_provider,
    };
    use std::sync::Arc;

    #[test]
    fn a_missing_or_malformed_unit_id_is_invalid() {
        let err = parse_id("test.unit", &serde_json::json!({})).unwrap_err();
        crate::erased::assert_invalid(&err, "missing field `id`");
        let err = parse_id("test.unit", &serde_json::json!({ "id": "" })).unwrap_err();
        crate::erased::assert_invalid(&err, "test.unit: id: ");
    }

    // Each test uses a UNIQUE (provider name, kind) pair so the process-global
    // registry doesn't cross-contaminate parallel tests.
    struct TestProvider {
        name: String,
        kind: String,
    }

    impl UnitProvider for TestProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            vec![KindDeclaration {
                kind: self.kind.clone(),
                backup_spec: None,
                verbs: vec![
                    VerbDecl::list(),
                    VerbDecl::detail(),
                    VerbDecl {
                        verb: Verb::Update,
                        query_schema: None,
                        actions: vec![ActionDecl {
                            action: "spin".into(),
                            payload_schema: None,
                            response_schema: None,
                        }],
                    },
                    VerbDecl {
                        verb: Verb::Create,
                        query_schema: None,
                        actions: vec![ActionDecl {
                            action: "forge".into(),
                            payload_schema: None,
                            response_schema: None,
                        }],
                    },
                    VerbDecl::delete(),
                ],
            }]
        }
        fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
            let (name, kind) = (self.name.clone(), self.kind.clone());
            Box::pin(async move {
                Ok(vec![UnitDescriptor {
                    id: uid(&name, &kind, "w1"),
                    verbs: vec![Verb::Detail, Verb::Update, Verb::Delete],
                    parent: None,
                }])
            })
        }
        fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
            let (name, kind) = (self.name.clone(), self.kind.clone());
            Box::pin(async move {
                Ok(match args {
                    VerbArgs::List(_) => VerbOutcome::Items(ItemsOutcome {
                        items: vec![ItemOutcome::new(uid(&name, &kind, "w1"), "{}".into())],
                        total: Some(1),
                    }),
                    VerbArgs::Update(u) => VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: format!(
                            "spin:{}:{}",
                            u.id.id,
                            u.caller.map_or("-".to_string(), |c| c.username)
                        ),
                    }),
                    VerbArgs::Create(c) => VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: format!(
                            "forge:{name}:{}:{}",
                            c.action,
                            c.caller.map_or("-".to_string(), |c| c.username)
                        ),
                    }),
                    _ => VerbOutcome::Action(ActionOutcome::default()),
                })
            })
        }
    }

    fn uid(manager: &str, kind: &str, id: &str) -> UnitId {
        UnitId {
            manager: manager.into(),
            kind: kind.into(),
            id: id.into(),
            name: id.into(),
        }
    }

    /// Register a uniquely-named provider exposing a uniquely-named kind.
    fn setup(tag: &str) -> (String, String) {
        let name = format!("usurf-{tag}");
        let kind = format!("wid_{tag}");
        register_provider(Arc::new(TestProvider {
            name: name.clone(),
            kind: kind.clone(),
        }));
        (name, kind)
    }

    #[test]
    fn ops_cover_every_verb_and_action() {
        let (name, kind) = setup("ops");
        let names: Vec<String> = unit_ops().into_iter().map(|o| o.name).collect();
        for want in ["list", "detail", "delete", "spin", "forge"] {
            let full = format!("{kind}.{want}");
            assert!(
                names.iter().any(|n| n == &full),
                "missing {full} in {names:?}"
            );
        }
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn mcp_defs_carry_typed_input_schema() {
        let (name, kind) = setup("mcp");
        let defs = unit_mcp_defs();
        let want = format!("{kind}.spin");
        let spin = defs
            .iter()
            .find(|d| d["name"] == want.as_str())
            .expect("spin def");
        // update op input requires an id (a UnitId)
        let schema = &spin["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["id"].is_object());
        assert_eq!(schema["required"][0], "id");
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_list_routes_and_scopes_kind() {
        let (name, kind) = setup("list");
        let out = unit_dispatch(&format!("{kind}.list"), &json!({}), None)
            .await
            .expect("is a unit op")
            .expect("ok");
        assert!(
            out["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["id"]["id"] == "w1")
        );
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_update_routes_to_owner_by_id() {
        let (name, kind) = setup("upd");
        let out = unit_dispatch(
            &format!("{kind}.spin"),
            &json!({ "id": uid(&name, &kind, "w1") }),
            None,
        )
        .await
        .expect("is a unit op")
        .expect("ok");
        assert_eq!(out["changed"], true);
        assert_eq!(out["message"], "spin:w1:-");
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn mutating_unit_ops_require_admin_and_reads_stay_open() {
        let (name, kind) = setup("role");
        for op in ["spin", "forge", "delete"] {
            let tool = format!("{kind}.{op}");
            assert_eq!(crate::tool_roles::required_role(&tool), "admin", "{tool}");
            assert!(crate::tool_roles::is_data_mutation(&tool), "{tool}");
            for role in ["member", "read", "viewer", ""] {
                assert!(
                    !crate::tool_roles::authorize(role, false, "admin", true),
                    "{role} must be refused {tool}"
                );
            }
        }
        for op in ["list", "detail"] {
            assert_eq!(
                crate::tool_roles::required_role(&format!("{kind}.{op}")),
                "any"
            );
        }
        assert!(unit::deregister_provider(&name));
        assert_eq!(
            crate::tool_roles::required_role(&format!("{kind}.spin")),
            "any"
        );
    }

    #[tokio::test]
    async fn unit_update_and_create_carry_the_verified_caller_never_the_host_operator() {
        let (name, kind) = setup("caller");
        let who = |n: &str| contract::CallerIdentity {
            user_id: format!("u_{n}"),
            username: n.into(),
            role: "admin".into(),
            can_mutate: false,
        };
        let cfg = contract::config::Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: contract::config::Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: std::env::temp_dir(),
            memory_root: std::env::temp_dir(),
            db_path: std::env::temp_dir().join("unit-surface-test.db"),
            ports: Default::default(),
        };
        let host = contract::ToolCtx::new(Arc::new(cfg)).with_auth(who("host_admin"));
        let args = json!({ "id": uid(&name, &kind, "w1") });
        let tool = format!("{kind}.spin");

        let out = crate::dispatch(&tool, args.clone(), &host).await.unwrap();
        assert_eq!(out["message"], "spin:w1:-");

        let mut per_req = host.clone();
        per_req.set_verified_caller(Some(who("alice")));
        let out = crate::dispatch(&tool, args, &per_req).await.unwrap();
        assert_eq!(out["message"], "spin:w1:alice");

        let forge = format!("{kind}.forge");
        let out = crate::dispatch(&forge, json!({}), &host).await.unwrap();
        assert_eq!(out["message"], format!("forge:{name}:forge:-"));
        let out = crate::dispatch(&forge, json!({}), &per_req).await.unwrap();
        assert_eq!(out["message"], format!("forge:{name}:forge:alice"));
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_create_infers_sole_provider() {
        let (name, kind) = setup("crt");
        let out = unit_dispatch(&format!("{kind}.forge"), &json!({}), None)
            .await
            .expect("is a unit op")
            .expect("ok");
        assert_eq!(out["message"], format!("forge:{name}:forge:-"));
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_unknown_name_returns_none() {
        assert!(
            unit_dispatch("not.a.unit.op", &json!({}), None)
                .await
                .is_none()
        );
        assert!(
            unit_dispatch("ghost_xyz.list", &json!({}), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn dispatch_update_missing_id_errors() {
        let (name, kind) = setup("mis");
        let err = unit_dispatch(&format!("{kind}.spin"), &json!({}), None)
            .await
            .expect("is a unit op")
            .expect_err("missing id");
        assert!(err.to_string().contains("id"), "got: {err}");
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn unit_owns_matches_only_registered_ops() {
        let (name, kind) = setup("own");
        assert!(unit_owns(&format!("{kind}.list")));
        assert!(!unit_owns(&format!("{kind}.nonexistent")));
        assert!(!unit_owns("containers.list"));
        assert!(unit::deregister_provider(&name));
    }

    // ── Rich provider: plugin-declared query/payload/response schemas + upsert ──

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct QExtra {
        flavor: String,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct PPayload {
        size: u32,
    }

    struct RichProvider {
        name: String,
        kind: String,
    }

    impl UnitProvider for RichProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            vec![KindDeclaration {
                kind: self.kind.clone(),
                backup_spec: None,
                verbs: vec![
                    VerbDecl {
                        verb: Verb::List,
                        query_schema: Some(schemars::schema_for!(QExtra)),
                        actions: vec![],
                    },
                    VerbDecl {
                        verb: Verb::Detail,
                        query_schema: Some(schemars::schema_for!(QExtra)),
                        actions: vec![],
                    },
                    VerbDecl {
                        verb: Verb::Create,
                        query_schema: None,
                        actions: vec![ActionDecl {
                            action: "build".into(),
                            payload_schema: Some(schemars::schema_for!(PPayload)),
                            response_schema: Some(schemars::schema_for!(PPayload)),
                        }],
                    },
                    VerbDecl {
                        verb: Verb::Upsert,
                        query_schema: None,
                        actions: vec![ActionDecl {
                            action: "sync".into(),
                            payload_schema: Some(schemars::schema_for!(PPayload)),
                            response_schema: None,
                        }],
                    },
                ],
            }]
        }
        fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
            Box::pin(async move {
                Ok(match args {
                    VerbArgs::Detail(d) => VerbOutcome::Item(ItemOutcome::new(d.id, "{}".into())),
                    VerbArgs::Upsert(u) => VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: format!("upsert:{}:{}", u.id.id, u.action),
                    }),
                    _ => VerbOutcome::Action(ActionOutcome::default()),
                })
            })
        }
    }

    fn setup_rich(tag: &str) -> (String, String) {
        let name = format!("rich-{tag}");
        let kind = format!("rid_{tag}");
        register_provider(Arc::new(RichProvider {
            name: name.clone(),
            kind: kind.clone(),
        }));
        (name, kind)
    }

    #[test]
    fn create_op_carries_declared_payload_and_response_schemas() {
        let (name, kind) = setup_rich("schema");
        let ops = unit_ops();
        let build = ops
            .iter()
            .find(|o| o.name == format!("{kind}.build"))
            .expect("build op");
        // Declared payload embeds under `payload`; response is the declared schema.
        assert!(build.input_schema["properties"]["payload"].is_object());
        assert_eq!(build.output_schema["properties"]["size"]["type"], "integer");
        // Sole provider ⇒ provider not required.
        assert_eq!(build.input_schema["required"], json!([]));
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn list_schema_merges_plugin_query_fields() {
        let (name, kind) = setup_rich("query");
        let ops = unit_ops();
        let list = ops
            .iter()
            .find(|o| o.name == format!("{kind}.list"))
            .expect("list op");
        // Base QueryArgs props plus the plugin-declared `flavor` field.
        assert!(list.input_schema["properties"]["query"]["properties"]["flavor"].is_object());
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_detail_routes_and_returns_item() {
        let (name, kind) = setup_rich("detail");
        let out = unit_dispatch(
            &format!("{kind}.detail"),
            &json!({ "id": uid(&name, &kind, "w1") }),
            None,
        )
        .await
        .expect("is a unit op")
        .expect("ok");
        assert_eq!(out["id"]["id"], "w1");
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_detail_invalid_query_errors() {
        let (name, kind) = setup_rich("badq");
        let err = unit_dispatch(
            &format!("{kind}.detail"),
            &json!({ "id": uid(&name, &kind, "w1"), "query": 42 }),
            None,
        )
        .await
        .expect("is a unit op")
        .expect_err("bad query");
        assert!(err.to_string().contains("query"), "got: {err}");
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_upsert_routes_with_action() {
        let (name, kind) = setup_rich("ups");
        let out = unit_dispatch(
            &format!("{kind}.sync"),
            &json!({ "id": uid(&name, &kind, "w1"), "payload": { "size": 3 } }),
            None,
        )
        .await
        .expect("is a unit op")
        .expect("ok");
        assert_eq!(out["changed"], true);
        assert_eq!(out["message"], "upsert:w1:sync");
        assert!(unit::deregister_provider(&name));
    }

    // ── Multi-provider: same kind exposed by two providers ──

    #[test]
    fn create_input_requires_provider_when_multiple() {
        let name_a = "multi-a".to_string();
        let name_b = "multi-b".to_string();
        let kind = "mid_create".to_string();
        register_provider(Arc::new(RichProvider {
            name: name_a.clone(),
            kind: kind.clone(),
        }));
        register_provider(Arc::new(RichProvider {
            name: name_b.clone(),
            kind: kind.clone(),
        }));
        let ops = unit_ops();
        let build = ops
            .iter()
            .find(|o| o.name == format!("{kind}.build"))
            .expect("build op");
        assert!(build.providers.len() >= 2);
        assert_eq!(build.input_schema["required"], json!(["provider"]));
        assert!(unit::deregister_provider(&name_a));
        assert!(unit::deregister_provider(&name_b));
    }

    #[tokio::test]
    async fn dispatch_create_multi_provider_needs_selector() {
        let name_a = "sel-a".to_string();
        let name_b = "sel-b".to_string();
        let kind = "sid_create".to_string();
        register_provider(Arc::new(RichProvider {
            name: name_a.clone(),
            kind: kind.clone(),
        }));
        register_provider(Arc::new(RichProvider {
            name: name_b.clone(),
            kind: kind.clone(),
        }));
        // No provider ⇒ ambiguous error.
        let err = unit_dispatch(&format!("{kind}.build"), &json!({}), None)
            .await
            .expect("is a unit op")
            .expect_err("ambiguous");
        assert!(err.to_string().contains("multiple providers"), "got: {err}");
        // Explicit provider ⇒ routes cleanly.
        let out = unit_dispatch(
            &format!("{kind}.build"),
            &json!({ "provider": name_a }),
            None,
        )
        .await
        .expect("is a unit op")
        .expect("ok");
        assert_eq!(out["changed"], false);
        assert!(unit::deregister_provider(&name_a));
        assert!(unit::deregister_provider(&name_b));
    }

    // ── CLI / catalog projections ──

    #[test]
    fn cli_commands_expose_kind_and_leaf_subcommands() {
        let (name, kind) = setup("cli");
        let cmds = unit_cli_commands();
        let kind_cmd = cmds
            .iter()
            .find(|c| c.get_name() == kind)
            .expect("kind command");
        let leaves: Vec<&str> = kind_cmd.get_subcommands().map(|s| s.get_name()).collect();
        for want in ["list", "detail", "delete", "spin", "forge"] {
            assert!(leaves.contains(&want), "missing {want} in {leaves:?}");
        }
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn a_static_kind_gains_only_the_unit_leaves_it_lacks() {
        let (name, kind) = setup("collide");
        let root = clap::Command::new("orca").subcommand(
            clap::Command::new(&*Box::leak(kind.clone().into_boxed_str()))
                .subcommand(clap::Command::new("list").about("static list")),
        );
        let (root, routes) = register_unit_commands(root, unit_ops());
        let kind_cmd = root.find_subcommand(&kind).expect("kind command");
        let leaves: Vec<&str> = kind_cmd.get_subcommands().map(|s| s.get_name()).collect();
        for want in ["list", "detail", "delete", "spin", "forge"] {
            assert!(leaves.contains(&want), "missing {want} in {leaves:?}");
        }
        assert_eq!(leaves.iter().filter(|l| **l == "list").count(), 1);
        let list = kind_cmd.find_subcommand("list").expect("list");
        assert_eq!(
            list.get_about().map(|a| a.to_string()).as_deref(),
            Some("static list")
        );
        assert!(!routes.contains(&format!("{kind}.list")), "{routes:?}");
        assert!(routes.contains(&format!("{kind}.spin")), "{routes:?}");
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn a_new_kind_routes_every_leaf() {
        let (name, kind) = setup("lockstep");
        let (root, routes) = register_unit_commands(clap::Command::new("orca"), unit_ops());
        let kind_cmd = root.find_subcommand(&kind).expect("kind command");
        for leaf in kind_cmd.get_subcommands() {
            let full = format!("{kind}.{}", leaf.get_name());
            assert!(routes.contains(&full), "{full} not routed: {routes:?}");
        }
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn kinds_from_dedups_and_lists_kinds() {
        let (name, kind) = setup("kinds");
        let ops = unit_ops();
        let kinds = unit_kinds_from(&ops);
        assert!(kinds.contains(&kind));
        // No duplicates even though the kind has many ops.
        let count = kinds.iter().filter(|k| **k == kind).count();
        assert_eq!(count, 1);
        assert!(unit::deregister_provider(&name));
    }

    #[test]
    fn catalog_json_is_array_of_ops() {
        let (name, kind) = setup("catjson");
        let cat = unit_catalog_json();
        let arr = cat.as_array().expect("array");
        assert!(
            arr.iter()
                .any(|o| o["name"] == format!("{kind}.list").as_str()),
            "catalog missing {kind}.list"
        );
        assert!(unit::deregister_provider(&name));
    }

    // ── Owner resolution: a bare name/id picks the manager that owns it ──

    struct OwnerProvider {
        name: String,
        kind: String,
        units: Vec<UnitId>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
        fail: bool,
        page: Option<usize>,
    }

    impl UnitProvider for OwnerProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            vec![KindDeclaration {
                kind: self.kind.clone(),
                backup_spec: None,
                verbs: vec![
                    VerbDecl::list(),
                    VerbDecl::detail(),
                    VerbDecl::update(vec![ActionDecl::new("start")]),
                    VerbDecl::delete(),
                ],
            }]
        }
        fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
            let (units, deleted, name) =
                (self.units.clone(), self.deleted.clone(), self.name.clone());
            let (fail, page) = (self.fail, self.page);
            Box::pin(async move {
                Ok(match args {
                    VerbArgs::List(_) if fail => anyhow::bail!("secret backend detail"),
                    VerbArgs::List(l) => {
                        let total = units.len();
                        let from = l.query.offset.unwrap_or(0) as usize;
                        let take = page.unwrap_or(total);
                        VerbOutcome::Items(ItemsOutcome {
                            items: units
                                .into_iter()
                                .skip(from)
                                .take(take)
                                .map(|id| ItemOutcome::new(id, "{}".into()))
                                .collect(),
                            total: page.map(|_| total as u64),
                        })
                    }
                    VerbArgs::Detail(d) => VerbOutcome::Item(ItemOutcome::new(d.id, "{}".into())),
                    VerbArgs::Update(u) => VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: format!("{name}:{}:{}", u.action, u.id.manager),
                    }),
                    VerbArgs::Delete(d) => {
                        deleted.lock().unwrap().push(d.id.id.clone());
                        VerbOutcome::Action(ActionOutcome {
                            changed: true,
                            message: format!("deleted:{}", d.id.manager),
                        })
                    }
                    _ => VerbOutcome::Action(ActionOutcome::default()),
                })
            })
        }
    }

    type Deleted = Arc<std::sync::Mutex<Vec<String>>>;

    fn units_of(manager: &str, kind: &str, units: &[(&str, &str)]) -> Vec<UnitId> {
        units
            .iter()
            .map(|(id, name)| UnitId {
                manager: manager.into(),
                kind: kind.into(),
                id: (*id).into(),
                name: (*name).into(),
            })
            .collect()
    }

    fn owner_with(
        base: &str,
        kind: &str,
        units: Vec<UnitId>,
        fail: bool,
        page: Option<usize>,
    ) -> Deleted {
        let deleted: Deleted = Arc::default();
        register_provider(Arc::new(OwnerProvider {
            name: base.into(),
            kind: kind.into(),
            units,
            deleted: deleted.clone(),
            fail,
            page,
        }));
        deleted
    }

    fn owner(base: &str, scope: &str, kind: &str, units: &[(&str, &str)]) -> Deleted {
        let manager = UnitId::scoped_manager(base, scope);
        owner_with(base, kind, units_of(&manager, kind, units), false, None)
    }

    #[tokio::test]
    async fn a_provider_claiming_a_foreign_managers_unit_is_ignored() {
        let kind = "own_foreign";
        owner("unraid-f", "tower", kind, &[("abc", "plex")]);
        let claimed = units_of("unraid-f@tower", kind, &[("abc", "plex"), ("zzz", "ghost")]);
        owner_with("docker-f", kind, claimed, false, None);
        let tool = format!("{kind}.start");
        let out = unit_dispatch(&tool, &json!({ "id": "plex" }), None)
            .await
            .expect("is a unit op")
            .expect("one owner");
        assert_eq!(out["message"], "unraid-f:start:unraid-f@tower");
        let err = unit_dispatch(&tool, &json!({ "id": "ghost" }), None)
            .await
            .expect("is a unit op")
            .expect_err("only the foreign claimant lists it");
        assert!(err.to_string().contains("no own_foreign 'ghost'"), "{err}");
        assert!(unit::deregister_provider("unraid-f"));
        assert!(unit::deregister_provider("docker-f"));
    }

    #[tokio::test]
    async fn an_unreachable_manager_refuses_mutations_but_not_detail() {
        let kind = "own_down";
        owner("unraid-d2", "tower", kind, &[("abc", "plex")]);
        owner_with("docker-d2", kind, vec![], true, None);
        let cases = [
            ("start", json!({ "id": "plex" })),
            ("delete", json!({ "id": "plex" })),
            ("delete", json!({ "id": "plex", "dryRun": true })),
        ];
        for (op, args) in cases {
            let err = unit_dispatch(&format!("{kind}.{op}"), &args, None)
                .await
                .expect("is a unit op")
                .expect_err("refused");
            let msg = err.to_string();
            assert!(
                msg.contains("unreachable") && msg.contains("docker-d2"),
                "{op}: {msg}"
            );
            assert!(!msg.contains("secret backend detail"), "{msg}");
        }
        let out = unit_dispatch(&format!("{kind}.detail"), &json!({ "id": "plex" }), None)
            .await
            .expect("is a unit op")
            .expect("detail is lenient");
        assert_eq!(out["id"]["manager"], "unraid-d2@tower");
        assert!(unit::deregister_provider("unraid-d2"));
        assert!(unit::deregister_provider("docker-d2"));
    }

    #[tokio::test]
    async fn resolution_follows_pages_until_total() {
        let kind = "own_page";
        let units = units_of(
            "unraid-p@tower",
            kind,
            &[("a", "one"), ("b", "two"), ("c", "three")],
        );
        owner_with("unraid-p", kind, units, false, Some(1));
        let out = unit_dispatch(&format!("{kind}.start"), &json!({ "id": "three" }), None)
            .await
            .expect("is a unit op")
            .expect("found on the last page");
        assert_eq!(out["message"], "unraid-p:start:unraid-p@tower");
        assert!(unit::deregister_provider("unraid-p"));
    }

    #[tokio::test]
    async fn the_gate_checks_the_verified_caller_over_the_ambient_operator() {
        let kind = "own_gate";
        owner("unraid-g", "tower", kind, &[("abc", "plex")]);
        let who = |n: &str, role: &str| contract::CallerIdentity {
            user_id: format!("u_{n}"),
            username: n.into(),
            role: role.into(),
            can_mutate: false,
        };
        let mut ctx = test_ctx().with_auth(who("host_admin", "admin"));
        ctx.set_verified_caller(Some(who("mallory", "member")));
        let err = crate::dispatch(&format!("{kind}.start"), json!({ "id": "plex" }), &ctx)
            .await
            .expect_err("member refused");
        assert!(err.to_string().contains("requires role 'admin'"), "{err}");
        assert!(unit::deregister_provider("unraid-g"));
    }

    #[tokio::test]
    async fn a_bare_name_routes_to_the_manager_that_owns_it() {
        let kind = "own_route";
        owner("unraid-r", "tower", kind, &[("abc123", "plex")]);
        owner("docker-r", "willow", kind, &[("def456", "sonarr")]);
        let tool = format!("{kind}.start");
        let out = unit_dispatch(&tool, &json!({ "id": "plex" }), None)
            .await
            .expect("is a unit op")
            .expect("ok");
        assert_eq!(out["message"], "unraid-r:start:unraid-r@tower");
        let out = unit_dispatch(&tool, &json!({ "id": "def456" }), None)
            .await
            .expect("is a unit op")
            .expect("ok");
        assert_eq!(out["message"], "docker-r:start:docker-r@willow");
        assert!(unit::deregister_provider("unraid-r"));
        assert!(unit::deregister_provider("docker-r"));
    }

    #[tokio::test]
    async fn a_name_two_managers_own_is_ambiguous_and_lists_them() {
        let kind = "own_amb";
        owner("unraid-a", "tower", kind, &[("abc", "plex")]);
        owner("docker-a", "willow", kind, &[("def", "plex")]);
        let err = unit_dispatch(&format!("{kind}.start"), &json!({ "id": "plex" }), None)
            .await
            .expect("is a unit op")
            .expect_err("ambiguous");
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(msg.contains("unraid-a@tower/abc"), "{msg}");
        assert!(msg.contains("docker-a@willow/def"), "{msg}");
        assert!(unit::deregister_provider("unraid-a"));
        assert!(unit::deregister_provider("docker-a"));
    }

    #[tokio::test]
    async fn a_name_no_manager_owns_lists_the_managers_checked() {
        let kind = "own_none";
        owner("unraid-n", "tower", kind, &[("abc", "plex")]);
        let err = unit_dispatch(&format!("{kind}.start"), &json!({ "id": "ghost" }), None)
            .await
            .expect("is a unit op")
            .expect_err("not found");
        let msg = err.to_string();
        assert!(msg.contains("no own_none 'ghost'"), "{msg}");
        assert!(msg.contains("unraid-n"), "{msg}");
        assert!(unit::deregister_provider("unraid-n"));
    }

    fn test_ctx() -> contract::ToolCtx {
        let cfg = contract::config::Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: contract::config::Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: std::env::temp_dir(),
            memory_root: std::env::temp_dir(),
            db_path: std::env::temp_dir().join("unit-surface-test.db"),
            ports: Default::default(),
        };
        contract::ToolCtx::new(Arc::new(cfg))
    }

    #[tokio::test]
    async fn delete_dry_run_resolves_the_owner_and_deletes_nothing() {
        let kind = "own_del";
        let deleted = owner("unraid-d", "tower", kind, &[("abc", "plex")]);
        let tool = format!("{kind}.delete");
        let ctx = test_ctx();
        let out = crate::dispatch(&tool, json!({ "id": "plex", "dryRun": true }), &ctx)
            .await
            .expect("dry run");
        assert_eq!(out["changed"], false);
        assert_eq!(
            out["message"],
            "dry run: would delete own_del 'plex' (abc) on unraid-d@tower"
        );
        assert!(deleted.lock().unwrap().is_empty());

        let out = crate::dispatch(&tool, json!({ "id": "plex" }), &ctx)
            .await
            .expect("delete");
        assert_eq!(out["message"], "deleted:unraid-d@tower");
        assert_eq!(*deleted.lock().unwrap(), vec!["abc".to_string()]);
        assert!(unit::deregister_provider("unraid-d"));
    }

    #[tokio::test]
    async fn delete_dry_run_of_a_missing_unit_is_not_found() {
        let kind = "own_delx";
        owner("unraid-x", "tower", kind, &[("abc", "plex")]);
        let ghost = UnitId {
            manager: "unraid-x@tower".into(),
            kind: kind.into(),
            id: "zzz".into(),
            name: "zzz".into(),
        };
        let err = crate::dispatch(
            &format!("{kind}.delete"),
            json!({ "id": ghost, "dryRun": true }),
            &test_ctx(),
        )
        .await
        .expect_err("missing");
        assert!(err.to_string().contains("no own_delx 'zzz'"), "{err}");
        assert!(unit::deregister_provider("unraid-x"));
    }

    #[tokio::test]
    async fn dry_run_on_a_non_delete_unit_op_is_still_refused() {
        let kind = "own_upd";
        owner("unraid-u", "tower", kind, &[("abc", "plex")]);
        let err = crate::dispatch(
            &format!("{kind}.start"),
            json!({ "id": "plex", "dryRun": true }),
            &test_ctx(),
        )
        .await
        .expect_err("refused");
        assert!(err.to_string().contains("does not support dryRun"), "{err}");
        assert!(unit::deregister_provider("unraid-u"));
    }

    #[test]
    fn delete_schema_advertises_dry_run_and_a_name_target() {
        let (name, kind) = setup("delschema");
        let ops = unit_ops();
        let del = ops
            .iter()
            .find(|o| o.name == format!("{kind}.delete"))
            .expect("delete op");
        assert_eq!(del.input_schema["properties"]["dryRun"]["type"], "boolean");
        assert_eq!(
            del.input_schema["properties"]["id"]["anyOf"][1]["type"],
            "string"
        );
        assert!(unit::deregister_provider(&name));
    }
}
