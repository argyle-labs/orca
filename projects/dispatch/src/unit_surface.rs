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

fn resolve(name: &str) -> Option<OpSpec> {
    op_specs().into_iter().find(|s| s.name == name)
}

/// True iff a loaded unit provider exposes an op with this name. Names are
/// `<kind>.<verb>` (no `unit.` prefix — that's internal), so ownership is
/// decided by live-catalog membership, not a name prefix.
pub fn unit_owns(name: &str) -> bool {
    op_specs().iter().any(|s| s.name == name)
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
                            input_schema: id_only_input_schema(),
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

/// `{ id: UnitId }` — target selector for delete.
fn id_only_input_schema() -> Value {
    let mut defs = Map::new();
    let id = embed(schema_value::<UnitId>(), &mut defs);
    finish(
        json!({
            "type": "object",
            "properties": { "id": id },
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
    let id = embed(schema_value::<UnitId>(), &mut defs);
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
    let id = embed(schema_value::<UnitId>(), &mut defs);
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
                "inputSchema": op.input_schema,
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
/// CLI commands the unit surface owns (`vm`, `lxc`, `container`, …). The CLI
/// uses this to know which top-level command names to route to the unit surface.
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
                );
            kind_cmd = kind_cmd.subcommand(leaf_cmd);
        }
        commands.push(kind_cmd);
    }
    commands
}

// ── Dispatch ────────────────────────────────────────────────────────────────────

/// Route a `unit.<kind>.<op>` call back through the contract-side dispatcher.
/// Returns `None` when `name` isn't a unit op (so the caller falls through).
pub async fn unit_dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    let spec = resolve(name)?;
    Some(run(spec, args).await)
}

async fn run(spec: OpSpec, args: &Value) -> Result<Value> {
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
            let id = parse_id(args)?;
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
            let id = parse_id(args)?;
            unit::dispatch(VerbArgs::Delete(DeleteArgs { id })).await?
        }
        Verb::Update => {
            let id = parse_id(args)?;
            let payload = args.get("payload").map(|v| v.to_string());
            let action = spec
                .action
                .clone()
                .ok_or_else(|| anyhow::anyhow!("update op missing action"))?;
            unit::dispatch(VerbArgs::Update(UpdateArgs {
                id,
                action,
                payload,
            }))
            .await?
        }
        Verb::Upsert => {
            let id = parse_id(args)?;
            let payload = args.get("payload").map(|v| v.to_string());
            let action = spec
                .action
                .clone()
                .ok_or_else(|| anyhow::anyhow!("upsert op missing action"))?;
            unit::dispatch(VerbArgs::Upsert(UpsertArgs {
                id,
                action,
                payload,
            }))
            .await?
        }
        Verb::Create => {
            let payload = args.get("payload").map(|v| v.to_string());
            let action = spec
                .action
                .clone()
                .ok_or_else(|| anyhow::anyhow!("create op missing action"))?;
            let call = CreateArgs { action, payload };
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

fn parse_id(args: &Value) -> Result<UnitId> {
    let id = args
        .get("id")
        .ok_or_else(|| anyhow::anyhow!("missing required \"id\" (a UnitId)"))?;
    serde_json::from_value(id.clone()).map_err(|e| anyhow::anyhow!("invalid id: {e}"))
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
                        message: format!("spin:{}", u.id.id),
                    }),
                    VerbArgs::Create(c) => VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: format!("forge:{name}:{}", c.action),
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
        let out = unit_dispatch(&format!("{kind}.list"), &json!({}))
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
        )
        .await
        .expect("is a unit op")
        .expect("ok");
        assert_eq!(out["changed"], true);
        assert_eq!(out["message"], "spin:w1");
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_create_infers_sole_provider() {
        let (name, kind) = setup("crt");
        let out = unit_dispatch(&format!("{kind}.forge"), &json!({}))
            .await
            .expect("is a unit op")
            .expect("ok");
        assert_eq!(out["message"], format!("forge:{name}:forge"));
        assert!(unit::deregister_provider(&name));
    }

    #[tokio::test]
    async fn dispatch_unknown_name_returns_none() {
        assert!(unit_dispatch("not.a.unit.op", &json!({})).await.is_none());
        assert!(unit_dispatch("ghost_xyz.list", &json!({})).await.is_none());
    }

    #[tokio::test]
    async fn dispatch_update_missing_id_errors() {
        let (name, kind) = setup("mis");
        let err = unit_dispatch(&format!("{kind}.spin"), &json!({}))
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
}
