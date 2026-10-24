#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use contract::{CallerIdentity, ToolCtx};
use openapi::surface::{ApiOp, ApiSurface, EndpointBinding, SurfaceKind};
use plugin_abi::{DbOp, DbRow, DbValue, SecretOp};
use serde::Serialize;
use serde_json::Value;
use utils::route::Route;

pub const ENDPOINT_ARG: &str = "endpoint";
const SHARED_ENDPOINTS: &str = "endpoints";
const REDACTED: &str = "***";
const MAX_ERROR_BODY: usize = 2048;

type Registry = BTreeMap<String, BTreeMap<String, Arc<ApiSurface>>>;

static SURFACES: RwLock<Registry> = RwLock::new(BTreeMap::new());

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub method: String,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub body: Value,
    pub endpoint: String,
    pub route: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "mode", content = "result", rename_all = "camelCase")]
pub enum Outcome {
    Plan(Plan),
    Response(Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    pub tool: String,
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: upstream http {}: {}",
            self.tool, self.status, self.body
        )
    }
}

impl std::error::Error for UpstreamError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forbidden {
    pub tool: String,
    pub reason: String,
}

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.tool, self.reason)
    }
}

impl std::error::Error for Forbidden {}

#[derive(Debug, Clone)]
pub struct Invocation {
    caller: CallerIdentity,
    dry_run: bool,
    local: bool,
    second_factor: bool,
    execute_confirmed: bool,
    reaches: Vec<String>,
    system: bool,
    execute_gated: bool,
    call_chain: Vec<String>,
    tool_calls: Vec<String>,
    call_budget: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

const SYSTEM_ROLE: &str = "read";

impl Invocation {
    pub fn from_ctx(ctx: &ToolCtx) -> Result<Self> {
        let caller = ctx
            .verified_caller()
            .cloned()
            .ok_or_else(|| anyhow!("api-surface calls require an authenticated caller"))?;
        Ok(Self {
            caller,
            dry_run: ctx.dry_run(),
            local: ctx.local_transport(),
            second_factor: ctx.second_factor(),
            execute_confirmed: false,
            reaches: Vec::new(),
            system: false,
            execute_gated: false,
            call_chain: ctx.call_chain().to_vec(),
            tool_calls: Vec::new(),
            call_budget: match ctx.call_budget() {
                Some(budget) => budget.clone(),
                None if !ctx.call_chain().is_empty() => {
                    std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX / 2))
                }
                None => Default::default(),
            },
        })
    }

    pub fn system() -> Self {
        Self {
            caller: CallerIdentity {
                user_id: String::new(),
                username: "orca-system".into(),
                role: SYSTEM_ROLE.into(),
                can_mutate: false,
            },
            dry_run: false,
            local: false,
            second_factor: false,
            execute_confirmed: false,
            reaches: Vec::new(),
            system: true,
            execute_gated: false,
            call_chain: Vec::new(),
            tool_calls: Vec::new(),
            call_budget: Default::default(),
        }
    }

    pub fn is_system(&self) -> bool {
        self.system
    }

    pub fn caller(&self) -> &CallerIdentity {
        &self.caller
    }

    pub fn child_may_execute(&self) -> bool {
        self.execute_gated && self.execute_confirmed && !self.dry_run
    }

    pub fn call_chain(&self) -> &[String] {
        &self.call_chain
    }

    pub fn call_budget(&self) -> &std::sync::Arc<std::sync::atomic::AtomicUsize> {
        &self.call_budget
    }

    pub fn tool_calls(&self) -> &[String] {
        &self.tool_calls
    }

    pub fn with_tool_calls(mut self, tool_calls: &[String]) -> Self {
        self.tool_calls = tool_calls.to_vec();
        self
    }

    pub fn for_plugin_invoke(
        mut self,
        outer_execute_gated: bool,
        outer_role: &str,
        outer_reaches: &[String],
        args: &Value,
    ) -> Self {
        self.reaches = outer_reaches.to_vec();
        self.execute_gated = outer_execute_gated;
        let flag = |k: &str| args.get(k) == Some(&Value::Bool(true));
        self.dry_run |= flag(contract::plan::DRY_RUN_FIELD);
        self.execute_confirmed =
            outer_execute_gated && !self.dry_run && flag(contract::plan::EXECUTE_FIELD);
        self.second_factor &= outer_role == "super-admin";
        self
    }

    pub fn for_backend(
        verb: &contract::invocation_scope::CoreVerb,
        targeted: bool,
        reaches: &[String],
    ) -> Self {
        let rank = dispatch::tool_roles::role_rank;
        let floor = if !targeted || rank(verb.role()) < rank("read") {
            "read"
        } else {
            verb.role()
        };
        let role = if rank(floor) < rank(&verb.caller().role) {
            floor.to_string()
        } else {
            verb.caller().role.clone()
        };
        let inv = Self {
            second_factor: verb.second_factor() && role == "super-admin",
            caller: CallerIdentity {
                user_id: verb.caller().user_id.clone(),
                username: verb.caller().username.clone(),
                role,
                can_mutate: verb.caller().can_mutate,
            },
            dry_run: verb.dry_run(),
            local: verb.local(),
            execute_confirmed: verb.execute_gated() && !verb.dry_run(),
            reaches: Vec::new(),
            system: false,
            execute_gated: verb.execute_gated(),
            call_chain: Vec::new(),
            tool_calls: Vec::new(),
            call_budget: Default::default(),
        };
        if targeted {
            inv.with_reaches(reaches)
        } else {
            inv.unconfirmed()
        }
    }

    pub fn unconfirmed(mut self) -> Self {
        self.execute_confirmed = false;
        self.reaches = Vec::new();
        self
    }

    pub fn with_reaches(mut self, reaches: &[String]) -> Self {
        self.reaches = reaches.to_vec();
        self
    }
}

pub fn register(plugin: &str, surface: ApiSurface) -> Result<()> {
    db::plugin_tables::validate_principal(plugin)?;
    let prefix = format!("{plugin}.");
    let mut seen = std::collections::BTreeSet::new();
    for op in &surface.ops {
        if !op.tool.starts_with(&prefix) || op.tool.len() == prefix.len() {
            bail!(
                "surface `{}`: tool `{}` is outside plugin `{plugin}`",
                surface.surface,
                op.tool
            );
        }
        if !seen.insert(op.tool.as_str()) {
            bail!(
                "surface `{}`: duplicate tool `{}`",
                surface.surface,
                op.tool
            );
        }
    }
    let mut reg = SURFACES
        .write()
        .map_err(|_| anyhow!("surface registry poisoned"))?;
    let owned = reg.entry(plugin.to_string()).or_default();
    for (name, other) in owned.iter() {
        if *name == surface.surface {
            continue;
        }
        if let Some(op) = other.ops.iter().find(|o| seen.contains(o.tool.as_str())) {
            bail!("tool `{}` already registered by surface `{name}`", op.tool);
        }
    }
    owned.insert(surface.surface.clone(), Arc::new(surface));
    Ok(())
}

pub fn deregister(plugin: &str) -> Vec<String> {
    let Ok(mut reg) = SURFACES.write() else {
        return Vec::new();
    };
    reg.remove(plugin)
        .into_iter()
        .flat_map(|s| s.into_values())
        .flat_map(|s| s.ops.iter().map(|o| o.tool.clone()).collect::<Vec<_>>())
        .collect()
}

fn lookup(plugin: &str, tool: &str) -> Result<(Arc<ApiSurface>, usize)> {
    lookup_in(plugin, None, tool)
}

fn lookup_in(plugin: &str, surface: Option<&str>, tool: &str) -> Result<(Arc<ApiSurface>, usize)> {
    let reg = SURFACES
        .read()
        .map_err(|_| anyhow!("surface registry poisoned"))?;
    reg.get(plugin)
        .into_iter()
        .flat_map(|s| s.values())
        .filter(|s| surface.is_none_or(|name| s.surface == name))
        .find_map(|s| {
            s.ops
                .iter()
                .position(|o| o.tool == tool)
                .map(|i| (s.clone(), i))
        })
        .ok_or_else(|| anyhow!("plugin `{plugin}` has no api-surface tool `{tool}`"))
}

fn canonical_role(role: &str) -> Option<&'static str> {
    match role {
        "super-admin" => Some("super-admin"),
        "admin" => Some("admin"),
        "user" | "member" => Some("user"),
        "read" | "read-only" => Some("read"),
        _ => None,
    }
}

pub fn required_role(kind: SurfaceKind, op: &ApiOp) -> &'static str {
    let declared = canonical_role(&op.policy.role);
    if kind == SurfaceKind::Openapi && op.method.eq_ignore_ascii_case("GET") {
        return declared.unwrap_or("admin");
    }
    match declared {
        Some("super-admin") => "super-admin",
        _ => "admin",
    }
}

fn gate(
    surface: &ApiSurface,
    op: &ApiOp,
    inv: &Invocation,
    dry_run: bool,
    execute_confirmed: bool,
) -> Result<()> {
    let required = required_role(surface.kind, op);
    let deny = |reason: String| -> Result<()> {
        Err(Forbidden {
            tool: op.tool.clone(),
            reason,
        }
        .into())
    };
    if inv.system && (required != SYSTEM_ROLE || surface.execute_gated(op)) {
        return deny("system callers may only read".into());
    }
    if !dispatch::tool_roles::satisfies(&inv.caller.role, required) {
        return deny(format!("requires role `{required}`"));
    }
    if op.policy.local_only && !inv.local {
        return deny("local callers only".into());
    }
    if required == "super-admin" && !inv.second_factor {
        return deny("requires a verified second factor".into());
    }
    if surface.execute_gated(op) && !dry_run && !execute_confirmed {
        return deny("requires explicit execute confirmation".into());
    }
    Ok(())
}

pub fn authorize_capability(cap: &str, inv: &Invocation) -> Result<()> {
    let deny = |reason: &str| -> Result<()> {
        Err(Forbidden {
            tool: cap.to_string(),
            reason: reason.to_string(),
        }
        .into())
    };
    if inv.system || !dispatch::tool_roles::satisfies(&inv.caller.role, "admin") {
        return deny("requires role `admin`");
    }
    if !inv.reaches.iter().any(|r| r == cap) {
        return deny("not declared in the invoking tool's reaches");
    }
    if inv.dry_run {
        return deny("refused under dry run");
    }
    Ok(())
}

pub async fn execute(plugin: &str, tool: &str, args: Value, inv: &Invocation) -> Result<Outcome> {
    let (surface, index) = lookup(plugin, tool)?;
    if surface.ops[index].internal {
        bail!("plugin `{plugin}` has no api-surface tool `{tool}`");
    }
    let Value::Object(args) = args else {
        bail!("{tool}: arguments must be an object");
    };
    run(plugin, &surface, index, args, inv).await
}

pub async fn call(
    plugin: &str,
    surface: Option<&str>,
    op: &str,
    endpoint: &str,
    args: Value,
    inv: &Invocation,
) -> Result<Outcome> {
    let (found, index) = lookup_in(plugin, surface, op)?;
    let target = &found.ops[index];
    let sensitive =
        found.execute_gated(target) || required_role(found.kind, target) == "super-admin";
    if sensitive && !inv.reaches.contains(&target.tool) {
        return Err(Forbidden {
            tool: target.tool.clone(),
            reason: "not declared in the invoking tool's reaches".into(),
        }
        .into());
    }
    let Value::Object(mut args) = args else {
        bail!("{op}: arguments must be an object");
    };
    for reserved in [
        ENDPOINT_ARG,
        contract::plan::EXECUTE_FIELD,
        contract::plan::DRY_RUN_FIELD,
    ] {
        if args.contains_key(reserved) {
            bail!("{op}: `{reserved}` is set by core, not the caller");
        }
    }
    args.insert(ENDPOINT_ARG.into(), Value::String(endpoint.to_string()));
    if inv.execute_confirmed {
        args.insert(contract::plan::EXECUTE_FIELD.into(), Value::Bool(true));
    }
    run(plugin, &found, index, args, inv).await
}

async fn run(
    plugin: &str,
    surface: &ApiSurface,
    index: usize,
    mut args: serde_json::Map<String, Value>,
    inv: &Invocation,
) -> Result<Outcome> {
    let op = &surface.ops[index];
    let tool = op.tool.as_str();
    let execute_confirmed = args.remove(contract::plan::EXECUTE_FIELD) == Some(Value::Bool(true));
    let dry_run =
        inv.dry_run || args.remove(contract::plan::DRY_RUN_FIELD) == Some(Value::Bool(true));
    gate(surface, op, inv, dry_run, execute_confirmed)?;

    let endpoint = match args.remove(ENDPOINT_ARG) {
        Some(Value::String(e)) => e,
        _ => bail!("{tool}: missing `{ENDPOINT_ARG}`"),
    };
    db::plugin_tables::validate_segment(ENDPOINT_ARG, &endpoint)?;
    surface.validate_args(op, &Value::Object(args.clone()))?;

    let row =
        utils::reactor::off_async_worker(|| endpoint_row(plugin, &surface.endpoint, &endpoint))?;
    let fields = &surface.endpoint.fields;
    if row.get(&fields.enabled).is_some_and(|v| !truthy(v)) {
        bail!("{tool}: endpoint `{endpoint}` is disabled");
    }
    let insecure = row.get(&fields.insecure).is_some_and(truthy);
    let routes = routes_of(&row, &fields.routes)
        .map_err(|e| anyhow!("{tool}: endpoint `{endpoint}`: {e:#}"))?;
    let columns =
        auth_columns(surface, &row).map_err(|e| anyhow!("{tool}: endpoint `{endpoint}`: {e:#}"))?;
    let route_key = format!("{plugin}.{endpoint}");
    let secret_name = surface
        .auth
        .as_ref()
        .map(|auth| auth.secret.replace("{endpoint}", &endpoint));

    if dry_run {
        if let Some(name) = &secret_name {
            let op = SecretOp::Exists { name: name.clone() };
            if !utils::reactor::off_async_worker(|| secret_op(plugin, &op))?.found {
                bail!("no secret named '{name}'");
            }
        }
        let base = utils::route::resolve_reachable(&route_key, &routes, |_| async { true }).await?;
        let secret = secret_name.as_ref().map(|_| REDACTED);
        let shown = columns
            .iter()
            .map(|(k, v)| {
                let v = if surface.is_public_column(k) {
                    v.as_str()
                } else {
                    REDACTED
                };
                (k.clone(), v.to_string())
            })
            .collect();
        let req = surface.build_request_with(op, &base, &args, secret, &shown)?;
        return Ok(Outcome::Plan(plan(req, endpoint, &base)));
    }

    let secret = match &secret_name {
        Some(name) => {
            let op = SecretOp::Get { name: name.clone() };
            let value = utils::reactor::off_async_worker(|| secret_op(plugin, &op))?.value;
            Some(value.ok_or_else(|| anyhow!("no secret named '{name}'"))?)
        }
        None => None,
    };
    let base = utils::route::resolve_reachable(&route_key, &routes, |url| async move {
        utils::route::probe_http(&url, insecure).await
    })
    .await?;
    let req = surface.build_request_with(op, &base, &args, secret.as_deref(), &columns)?;
    let builder = utils::http::shared()
        .raw_request(utils::http::RawRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
            timeout_ms: None,
            insecure,
            ..Default::default()
        })
        .map_err(|e| anyhow!("{tool}: {e}"))?;
    let resp = builder
        .send_raw()
        .await
        .map_err(|e| anyhow!("{tool}: {e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(UpstreamError {
            tool: op.tool.clone(),
            status: resp.status,
            body: scrub_body(&resp.body, secret.as_deref()),
        }
        .into());
    }
    surface
        .decode_response(op, &resp.body)
        .map(Outcome::Response)
}

fn endpoint_row(plugin: &str, binding: &EndpointBinding, endpoint: &str) -> Result<DbRow> {
    let op = DbOp::Get {
        namespace: if binding.table == SHARED_ENDPOINTS {
            String::new()
        } else {
            plugin.to_string()
        },
        table: binding.table.clone(),
        key_col: binding.key.clone(),
        key: endpoint.to_string(),
    };
    db::pool::with_pooled_or_open(|conn| {
        db::plugin_db_policy::exec_db_op_policed(conn, &op, plugin)
    })?
    .rows
    .into_iter()
    .next()
    .ok_or_else(|| anyhow!("plugin `{plugin}` has no endpoint `{endpoint}`"))
}

fn auth_columns(surface: &ApiSurface, row: &DbRow) -> Result<BTreeMap<String, String>> {
    surface
        .auth_columns()?
        .into_iter()
        .map(|col| {
            let value = match row.get(col).or_else(|| row.get(shared_column(col))) {
                Some(DbValue::Text(t)) => t.clone(),
                Some(DbValue::Int(i)) => i.to_string(),
                _ => match shared_config_field(row, col) {
                    Some(v) => v,
                    None => bail!("no `{col}` column"),
                },
            };
            Ok((col.to_string(), value))
        })
        .collect()
}

fn shared_column(field: &str) -> &str {
    match field {
        "token_id" => "auth_principal",
        other => other,
    }
}

fn shared_config_field(row: &DbRow, field: &str) -> Option<String> {
    let Some(DbValue::Text(cfg)) = row.get("config") else {
        return None;
    };
    match serde_json::from_str::<Value>(cfg).ok()?.get(field)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn truthy(v: &DbValue) -> bool {
    match v {
        DbValue::Int(i) => *i != 0,
        DbValue::Bool(b) => *b,
        DbValue::Text(t) => matches!(t.as_str(), "1" | "true"),
        _ => false,
    }
}

fn routes_of(row: &DbRow, column: &str) -> Result<Vec<Route>> {
    match row.get(column) {
        Some(DbValue::Text(t)) => {
            serde_json::from_str(t).context("routes column is not a route list")
        }
        _ => bail!("no `{column}` routes column"),
    }
}

fn secret_op(plugin: &str, op: &SecretOp) -> Result<plugin_abi::SecretReply> {
    secrets::exec_secret_op_pooled(&secrets::Principal::Plugin(plugin.to_string()), op)
}

fn plan(req: openapi::surface::BuiltRequest, endpoint: String, base: &str) -> Plan {
    let (url, query) = match req.url.split_once('?') {
        Some((u, q)) => (u.to_string(), parse_query(q)),
        None => (req.url.clone(), Vec::new()),
    };
    let body = if req.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&req.body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&req.body).into_owned()))
    };
    Plan {
        method: req.method,
        url: strip_userinfo(&url),
        query,
        body: utils::scrub::redact_json(body),
        endpoint,
        route: strip_userinfo(base),
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let decode = |s: &str| utils::url::decode(s).unwrap_or_else(|_| s.to_string());
            (decode(k), decode(v))
        })
        .collect()
}

fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{REDACTED}@{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

fn scrub_body(body: &[u8], secret: Option<&str>) -> String {
    let mut text = String::from_utf8_lossy(body).into_owned();
    if let Some(s) = secret.filter(|s| !s.is_empty()) {
        let b64 = utils::encoding::base64_encode(s.as_bytes());
        let b64url = utils::encoding::base64url_encode(s.as_bytes());
        for form in [
            s.to_string(),
            utils::url::encode(s),
            b64.trim_end_matches('=').to_string(),
            b64url.trim_end_matches('=').to_string(),
        ] {
            text = text.replace(&form, REDACTED);
        }
    }
    let text = match serde_json::from_str::<Value>(&text) {
        Ok(v) => utils::scrub::redact_json(v).to_string(),
        Err(_) => utils::scrub::scrub_line(&text).into_owned(),
    };
    match text.char_indices().nth(MAX_ERROR_BODY) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn caller(role: &str, can_mutate: bool) -> CallerIdentity {
        CallerIdentity {
            user_id: "u".into(),
            username: "u".into(),
            role: role.into(),
            can_mutate,
        }
    }

    fn ctx(caller: &CallerIdentity, dry_run: bool, local: bool, second_factor: bool) -> ToolCtx {
        use contract::config::{Config, Model};
        let mut ctx = ToolCtx::new(std::sync::Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: "/tmp".into(),
            memory_root: "/tmp".into(),
            db_path: "/tmp/test.db".into(),
            ports: Default::default(),
        }));
        ctx.set_verified_caller(Some(caller.clone()));
        ctx.set_dry_run(dry_run);
        ctx.set_local_transport(local);
        ctx.set_second_factor(second_factor);
        ctx
    }

    fn inv(caller: &CallerIdentity, dry_run: bool) -> Invocation {
        Invocation::from_ctx(&ctx(caller, dry_run, true, false)).unwrap()
    }

    fn reached(caller: &CallerIdentity) -> Invocation {
        let mut i = inv(caller, true);
        i.reaches = vec!["callp.create".into()];
        i
    }

    fn op(method: &str, role: &str, data_mutation: bool) -> ApiOp {
        serde_json::from_value(json!({
            "tool": "p.t",
            "method": method,
            "path": "/",
            "policy": {"role": role, "data_mutation": data_mutation}
        }))
        .unwrap()
    }

    fn surface(plugin: &str, secret: &str) -> ApiSurface {
        ApiSurface::parse(
            &json!({
                "format": openapi::surface::FORMAT,
                "surface": "main",
                "kind": "openapi",
                "base": {"api_root": "/api"},
                "auth": {
                    "scheme": "header",
                    "header": "Authorization",
                    "template": "Bearer {secret}",
                    "secret": secret
                },
                "response": {"envelope": "data"},
                "ops": [
                    {
                        "tool": format!("{plugin}.item"),
                        "method": "GET",
                        "path": "/items/{id}",
                        "params": [{"name": "id", "in": "path", "schema": {"type": "integer"}}],
                        "policy": {"role": "read"}
                    },
                    {
                        "tool": format!("{plugin}.create"),
                        "method": "POST",
                        "path": "/items",
                        "params": [
                            {"name": "name", "in": "body", "required": true},
                            {"name": "password", "in": "body"}
                        ],
                        "policy": {"role": "read", "data_mutation": true}
                    }
                ]
            })
            .to_string(),
        )
        .unwrap()
    }

    fn seed(plugin: &str, server: &MockServer, secret_value: &str) {
        let port = server.address().port();
        let routes =
            json!([{"kind": "lan_v4", "scheme": "http", "value": "127.0.0.1", "port": port}])
                .to_string();
        db::pool::with_pooled_or_open(|conn| {
            conn.execute(
                "INSERT INTO endpoints (id, provider, name, routes, enabled) VALUES (?1, ?2, 'main', ?3, 1)",
                [plugin, plugin, routes.as_str()],
            )?;
            conn.execute(
                "INSERT INTO endpoints (id, provider, name, routes, enabled) VALUES ('x-other', 'other', 'main', '[]', 1)",
                [],
            )?;
            let name = format!("{plugin}.main.token");
            secrets::upsert(conn, &name, "inline", "", None)?;
            secrets::write_inline_value(conn, &name, secret_value)?;
            secrets::upsert(conn, "other.main.token", "inline", "", None)?;
            secrets::write_inline_value(conn, "other.main.token", "foreign")?;
            Ok(())
        })
        .unwrap();
    }

    async fn in_temp_db<F: std::future::Future>(f: F) -> F::Output {
        let dir = tempfile::tempdir().unwrap();
        db::with_db_path(dir.path().join("orca.db"), f).await
    }

    #[test]
    fn floor_forces_admin_on_writes_and_unknown_get_roles() {
        let k = SurfaceKind::Openapi;
        assert_eq!(required_role(k, &op("GET", "read", false)), "read");
        assert_eq!(required_role(k, &op("get", "user", false)), "user");
        assert_eq!(required_role(k, &op("GET", "any", false)), "admin");
        assert_eq!(required_role(k, &op("GET", "", false)), "admin");
        assert_eq!(required_role(k, &op("POST", "read", true)), "admin");
        assert_eq!(required_role(k, &op("DELETE", "any", true)), "admin");
        assert_eq!(
            required_role(k, &op("PUT", "super-admin", true)),
            "super-admin"
        );
        assert_eq!(
            required_role(SurfaceKind::Graphql, &op("POST", "read", false)),
            "admin"
        );
    }

    #[test]
    fn gate_denies_by_default_for_flagged_ops() {
        let s = surface("g", "g.{endpoint}.token");
        let admin = caller("admin", false);
        let mut gated = op("POST", "admin", false);
        gated.policy.execute_gated = true;
        assert!(gate(&s, &gated, &inv(&admin, false), true, false).is_ok());
        assert!(gate(&s, &gated, &inv(&admin, false), false, false).is_err());
        assert!(gate(&s, &gated, &inv(&admin, false), false, true).is_ok());

        let mut local = op("GET", "read", false);
        local.policy.local_only = true;
        let remote = Invocation::from_ctx(&ctx(&admin, false, false, false)).unwrap();
        assert!(gate(&s, &local, &remote, false, false).is_err());
        assert!(gate(&s, &local, &inv(&admin, false), false, false).is_ok());

        let sensitive = op("POST", "super-admin", false);
        let root = caller("super-admin", false);
        assert!(gate(&s, &sensitive, &inv(&root, true), true, false).is_err());
        let verified = Invocation::from_ctx(&ctx(&root, true, true, true)).unwrap();
        assert!(gate(&s, &sensitive, &verified, true, false).is_ok());
        let admin_2fa = Invocation::from_ctx(&ctx(&admin, true, true, true)).unwrap();
        assert!(gate(&s, &sensitive, &admin_2fa, true, false).is_err());
    }

    #[test]
    fn system_caller_may_only_read() {
        let s = surface("g", "g.{endpoint}.token");
        let sys = Invocation::system();
        assert!(sys.is_system());
        assert!(gate(&s, &op("GET", "read", false), &sys, false, false).is_ok());
        assert!(gate(&s, &op("GET", "user", false), &sys, false, false).is_err());
        assert!(gate(&s, &op("GET", "admin", false), &sys, false, false).is_err());
        assert!(gate(&s, &op("GET", "super-admin", false), &sys, false, false).is_err());
        let mut gated_read = op("GET", "read", false);
        gated_read.policy.execute_gated = true;
        assert!(gate(&s, &gated_read, &sys, false, true).is_err());
        assert!(gate(&s, &gated_read, &sys, true, false).is_err());
        for method in ["POST", "PUT", "DELETE"] {
            assert!(gate(&s, &op(method, "read", true), &sys, false, true).is_err());
        }
        let reached = Invocation::system().with_reaches(&["vm.exec".to_string()]);
        assert!(authorize_capability("vm.exec", &reached).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn system_caller_api_call_refused_on_writes() {
        register("sysc", surface("sysc", "sysc.{endpoint}.token")).unwrap();
        let sys = Invocation::system().with_reaches(&["sysc.create".to_string()]);
        let err = call(
            "sysc",
            None,
            "sysc.create",
            "main",
            json!({"name": "n"}),
            &sys,
        )
        .await
        .unwrap_err();
        assert!(err.downcast_ref::<Forbidden>().is_some(), "{err}");
        deregister("sysc");
    }

    #[test]
    fn writes_are_gated_whatever_the_table_says() {
        let s = surface("g", "g.{endpoint}.token");
        let admin = caller("admin", false);
        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let write = op(method, "admin", false);
            assert!(!write.policy.execute_gated);
            assert!(gate(&s, &write, &inv(&admin, false), false, false).is_err());
            assert!(gate(&s, &write, &inv(&admin, false), false, true).is_ok());
        }
        assert!(
            gate(
                &s,
                &op("GET", "admin", false),
                &inv(&admin, false),
                false,
                false
            )
            .is_ok()
        );
        let mut g = s.clone();
        g.kind = SurfaceKind::Graphql;
        let mut mutation = op("POST", "admin", false);
        mutation.path = "mutation M { x }".into();
        assert!(gate(&g, &mutation, &inv(&admin, false), false, false).is_err());
        mutation.path = "fragment F on X { y } mutation M { x }".into();
        assert!(gate(&g, &mutation, &inv(&admin, false), false, false).is_err());
        let mut query = op("POST", "admin", false);
        query.path = "query Q { x }".into();
        assert!(gate(&g, &query, &inv(&admin, false), false, false).is_ok());
    }

    #[test]
    fn invocation_requires_verified_caller_and_defaults_closed() {
        let mut bare = ctx(&caller("admin", false), false, true, true);
        bare.set_verified_caller(None);
        assert!(Invocation::from_ctx(&bare).is_err());
        let mut fresh = ctx(&caller("admin", false), false, false, false);
        fresh.set_local_transport(false);
        let inv = Invocation::from_ctx(&fresh).unwrap();
        assert!(!inv.local && !inv.second_factor);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn args_cannot_grant_local_or_second_factor() {
        let mut s = surface("spoof", "spoof.{endpoint}.token");
        s.ops[0].policy.local_only = true;
        s.ops[1].policy.role = "super-admin".into();
        register("spoof", s).unwrap();
        let root = caller("super-admin", false);
        let remote = Invocation::from_ctx(&ctx(&root, true, false, false)).unwrap();
        for (tool, extra) in [
            (
                "spoof.item",
                json!({"endpoint": "main", "id": 1, "local": true, "localTransport": true}),
            ),
            (
                "spoof.create",
                json!({"endpoint": "main", "name": "n", "secondFactor": true, "second_factor": true}),
            ),
        ] {
            let err = execute("spoof", tool, extra, &remote).await.unwrap_err();
            assert!(err.downcast_ref::<Forbidden>().is_some(), "{tool}: {err}");
        }
        deregister("spoof");
    }

    #[test]
    fn register_rejects_foreign_and_duplicate_tools() {
        let s = surface("regp", "regp.{endpoint}.token");
        assert!(register("other", s.clone()).is_err());
        register("regp", s.clone()).unwrap();
        register("regp", s.clone()).unwrap();
        let mut second = s.clone();
        second.surface = "second".into();
        assert!(register("regp", second).is_err());
        let mut dup = s;
        dup.surface = "dup".into();
        dup.ops.push(dup.ops[0].clone());
        assert!(register("regp", dup).is_err());
        let mut removed = deregister("regp");
        removed.sort();
        assert_eq!(removed, vec!["regp.create", "regp.item"]);
        assert!(deregister("regp").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn executes_end_to_end_against_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/5"))
            .and(header("Authorization", "Bearer s3cr3t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 5}})))
            .mount(&server)
            .await;
        register("e2e", surface("e2e", "e2e.{endpoint}.token")).unwrap();
        let out = in_temp_db(async {
            seed("e2e", &server, "s3cr3t");
            execute(
                "e2e",
                "e2e.item",
                json!({"endpoint": "main", "id": 5}),
                &inv(&caller("read", false), false),
            )
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, Outcome::Response(json!({"id": 5})));
        deregister("e2e");
    }

    fn column_auth(plugin: &str, scheme: &str) -> ApiSurface {
        column_auth_with(plugin, scheme, json!([]))
    }

    fn column_auth_with(plugin: &str, scheme: &str, public: Value) -> ApiSurface {
        let mut s = surface(plugin, &format!("{plugin}.{{endpoint}}.token"));
        let mut value = serde_json::to_value(&s).unwrap();
        value["auth"]["scheme"] = json!(scheme);
        value["auth"]["template"] = json!("Tok={endpoint.provider}={secret}");
        value["endpoint"]["columns"] = json!(["provider"]);
        value["endpoint"]["public_columns"] = public;
        s = ApiSurface::parse(&value.to_string()).unwrap();
        s
    }

    fn token_id_surface() -> ApiSurface {
        let mut value = serde_json::to_value(column_auth("tokp", "header")).unwrap();
        value["auth"]["template"] = json!("PVEAPIToken={endpoint.token_id}={secret}");
        value["endpoint"]["columns"] = json!(["token_id"]);
        ApiSurface::parse(&value.to_string()).unwrap()
    }

    #[test]
    fn shared_row_auth_principal_satisfies_token_id() {
        let row = DbRow::from([(
            "auth_principal".to_string(),
            DbValue::Text("root@pam!orca".into()),
        )]);
        let cols = auth_columns(&token_id_surface(), &row).unwrap();
        assert_eq!(
            cols.get("token_id").map(String::as_str),
            Some("root@pam!orca")
        );
    }

    #[test]
    fn literal_column_wins_over_shared_mapping() {
        let row = DbRow::from([
            ("token_id".to_string(), DbValue::Text("lit".into())),
            ("auth_principal".to_string(), DbValue::Text("shared".into())),
        ]);
        let cols = auth_columns(&token_id_surface(), &row).unwrap();
        assert_eq!(cols.get("token_id").map(String::as_str), Some("lit"));
    }

    #[test]
    fn missing_auth_column_names_the_column() {
        let err = auth_columns(&token_id_surface(), &DbRow::new()).unwrap_err();
        let wrapped = anyhow!("tokp.item: endpoint `main`: {err:#}");
        assert!(format!("{wrapped:#}").contains("no `token_id` column"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_template_uses_endpoint_row_column() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/5"))
            .and(header("Authorization", "Tok=colp=s3cr3t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 5}})))
            .mount(&server)
            .await;
        register("colp", column_auth("colp", "header")).unwrap();
        let out = in_temp_db(async {
            seed("colp", &server, "s3cr3t");
            execute(
                "colp",
                "colp.item",
                json!({"endpoint": "main", "id": 5}),
                &inv(&caller("read", false), false),
            )
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, Outcome::Response(json!({"id": 5})));
        deregister("colp");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dry_run_plan_redacts_secret_and_private_columns() {
        for (plugin, public, shown) in [
            ("colq", json!([]), false),
            ("colp2", json!(["provider"]), true),
        ] {
            let server = MockServer::start().await;
            register(plugin, column_auth_with(plugin, "query", public)).unwrap();
            let out = in_temp_db(async {
                seed(plugin, &server, "s3cr3t");
                execute(
                    plugin,
                    &format!("{plugin}.item"),
                    json!({"endpoint": "main", "id": 5}),
                    &inv(&caller("read", false), true),
                )
                .await
            })
            .await
            .unwrap();
            let Outcome::Plan(plan) = out else {
                panic!("expected plan")
            };
            let text = serde_json::to_string(&plan).unwrap();
            assert!(!text.contains("s3cr3t"), "{text}");
            assert_eq!(text.contains(plugin), shown, "{text}");
            assert!(server.received_requests().await.unwrap().is_empty());
            deregister(plugin);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_error_carries_status_and_scrubbed_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/1"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"error": "bad key s3cr3t", "token": "abc"})),
            )
            .mount(&server)
            .await;
        register("uperr", surface("uperr", "uperr.{endpoint}.token")).unwrap();
        let err = in_temp_db(async {
            seed("uperr", &server, "s3cr3t");
            execute(
                "uperr",
                "uperr.item",
                json!({"endpoint": "main", "id": 1}),
                &inv(&caller("admin", false), false),
            )
            .await
        })
        .await
        .unwrap_err();
        let up = err.downcast_ref::<UpstreamError>().unwrap();
        assert_eq!(up.status, 403);
        assert!(
            !up.body.contains("s3cr3t") && !up.body.contains("abc"),
            "{}",
            up.body
        );
        deregister("uperr");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dry_run_plans_without_sending() {
        let server = MockServer::start().await;
        register("dry", surface("dry", "dry.{endpoint}.token")).unwrap();
        let out = in_temp_db(async {
            seed("dry", &server, "s3cr3t");
            execute(
                "dry",
                "dry.create",
                json!({"endpoint": "main", "name": "n", "password": "pw"}),
                &inv(&caller("admin", false), true),
            )
            .await
        })
        .await
        .unwrap();
        let Outcome::Plan(plan) = out else {
            panic!("expected plan")
        };
        assert_eq!(plan.method, "POST");
        assert!(plan.url.ends_with("/api/items"));
        assert_eq!(plan.endpoint, "main");
        assert_eq!(plan.body["name"], json!("n"));
        assert_ne!(plan.body["password"], json!("pw"));
        assert!(!serde_json::to_string(&plan).unwrap().contains("s3cr3t"));
        assert!(server.received_requests().await.unwrap().is_empty());
        deregister("dry");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn floor_denies_before_any_work() {
        let server = MockServer::start().await;
        register("deny", surface("deny", "deny.{endpoint}.token")).unwrap();
        let res = in_temp_db(async {
            seed("deny", &server, "s3cr3t");
            let write = execute(
                "deny",
                "deny.create",
                json!({"endpoint": "main", "name": "n"}),
                &inv(&caller("read", false), true),
            )
            .await;
            let mutate = execute(
                "deny",
                "deny.create",
                json!({"endpoint": "main", "name": "n"}),
                &inv(&caller("member", true), true),
            )
            .await;
            let read_mutate = execute(
                "deny",
                "deny.create",
                json!({"endpoint": "main", "name": "n"}),
                &inv(&caller("read", true), false),
            )
            .await;
            (write, mutate, read_mutate)
        })
        .await;
        assert!(res.0.unwrap_err().downcast_ref::<Forbidden>().is_some());
        assert!(res.1.unwrap_err().downcast_ref::<Forbidden>().is_some());
        assert!(res.2.unwrap_err().downcast_ref::<Forbidden>().is_some());
        assert!(server.received_requests().await.unwrap().is_empty());
        deregister("deny");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn foreign_secret_and_endpoint_are_refused() {
        let server = MockServer::start().await;
        register("own", surface("own", "other.{endpoint}.token")).unwrap();
        let res = in_temp_db(async {
            seed("own", &server, "s3cr3t");
            let secret = execute(
                "own",
                "own.item",
                json!({"endpoint": "main", "id": 1}),
                &inv(&caller("admin", false), false),
            )
            .await;
            let endpoint = execute(
                "own",
                "own.item",
                json!({"endpoint": "missing", "id": 1}),
                &inv(&caller("admin", false), false),
            )
            .await;
            let planned = execute(
                "own",
                "own.item",
                json!({"endpoint": "main", "id": 1}),
                &inv(&caller("admin", false), true),
            )
            .await;
            let bad_name = execute(
                "own",
                "own.item",
                json!({"endpoint": "../x", "id": 1}),
                &inv(&caller("admin", false), true),
            )
            .await;
            (secret, endpoint, planned, bad_name)
        })
        .await;
        assert!(
            res.2
                .unwrap_err()
                .downcast_ref::<secrets::SecretAccessDenied>()
                .is_some()
        );
        assert!(res.3.is_err());
        assert!(
            res.0
                .unwrap_err()
                .downcast_ref::<secrets::SecretAccessDenied>()
                .is_some()
        );
        assert!(
            res.1
                .unwrap_err()
                .to_string()
                .contains("no endpoint `missing`")
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        deregister("own");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_missing_endpoint_and_unknown_args() {
        register("args", surface("args", "args.{endpoint}.token")).unwrap();
        let c = caller("admin", false);
        let missing = execute("args", "args.item", json!({"id": 1}), &inv(&c, true)).await;
        assert!(
            missing
                .unwrap_err()
                .to_string()
                .contains("missing `endpoint`")
        );
        let unknown = execute(
            "args",
            "args.item",
            json!({"endpoint": "main", "id": 1, "x": 1}),
            &inv(&c, true),
        )
        .await;
        assert!(
            unknown
                .unwrap_err()
                .to_string()
                .contains("unexpected property `x`")
        );
        deregister("args");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn call_executes_end_to_end() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/7"))
            .and(header("Authorization", "Bearer k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 7}})))
            .mount(&server)
            .await;
        register("callok", surface("callok", "callok.{endpoint}.token")).unwrap();
        let out = in_temp_db(async {
            seed("callok", &server, "k");
            call(
                "callok",
                Some("main"),
                "callok.item",
                "main",
                json!({"id": 7}),
                &inv(&caller("read", false), false),
            )
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, Outcome::Response(json!({"id": 7})));
        deregister("callok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn call_runs_gated_ops_only_under_a_confirmed_gated_outer_tool() {
        let mut s = surface("gatep", "gatep.{endpoint}.token");
        s.ops[1].policy.execute_gated = true;
        register("gatep", s).unwrap();
        let admin = caller("admin", false);
        let args = || json!({"name": "n"});
        let executing = json!({"execute": true});
        let reach = vec!["gatep.create".to_string()];

        let plain = inv(&admin, false).for_plugin_invoke(false, "admin", &reach, &executing);
        let err = call("gatep", None, "gatep.create", "main", args(), &plain)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Forbidden>().is_some(), "{err}");

        let unconfirmed = inv(&admin, false).for_plugin_invoke(true, "admin", &reach, &json!({}));
        let err = call("gatep", None, "gatep.create", "main", args(), &unconfirmed)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Forbidden>().is_some(), "{err}");

        let confirmed = inv(&admin, false).for_plugin_invoke(true, "admin", &reach, &executing);
        let err = call("gatep", None, "gatep.create", "main", args(), &confirmed)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Forbidden>().is_none(), "{err}");

        let dry = inv(&admin, false).for_plugin_invoke(
            true,
            "admin",
            &reach,
            &json!({"execute": true, "dryRun": true}),
        );
        assert!(dry.dry_run && !dry.execute_confirmed);

        let outside = vec!["gatep.item".to_string()];
        let unreached = inv(&admin, false).for_plugin_invoke(true, "admin", &outside, &executing);
        let err = call("gatep", None, "gatep.create", "main", args(), &unreached)
            .await
            .unwrap_err();
        let forbidden = err.downcast_ref::<Forbidden>().expect("forbidden");
        assert!(forbidden.reason.contains("reaches"), "{err}");

        let ungated = call(
            "gatep",
            None,
            "gatep.item",
            "main",
            json!({"id": 1}),
            &unreached,
        )
        .await
        .unwrap_err();
        assert!(ungated.downcast_ref::<Forbidden>().is_none(), "{ungated}");
        deregister("gatep");
    }

    #[test]
    fn untargeted_backend_under_an_admin_verb_only_reads() {
        let verb = contract::invocation_scope::testing::verb(
            contract::invocation_scope::VerbCaller {
                user_id: "u".into(),
                username: "u".into(),
                role: "admin".into(),
                can_mutate: true,
            },
            "admin",
            false,
            false,
            true,
            false,
        );
        let s = surface("untg", "untg.{endpoint}.token");
        let untargeted = Invocation::for_backend(&verb, false, &[]);
        assert_eq!(untargeted.caller.role, "read");
        let err = gate(&s, &op("GET", "admin", false), &untargeted, false, false).unwrap_err();
        assert!(err.downcast_ref::<Forbidden>().is_some(), "{err}");
        gate(&s, &op("GET", "read", false), &untargeted, false, false).unwrap();

        let targeted = Invocation::for_backend(&verb, true, &[]);
        assert_eq!(targeted.caller.role, "admin");
        gate(&s, &op("GET", "admin", false), &targeted, false, false).unwrap();
    }

    #[test]
    fn second_factor_carries_only_from_a_super_admin_outer_tool() {
        let root = caller("super-admin", false);
        let verified = Invocation::from_ctx(&ctx(&root, false, true, true)).unwrap();
        assert!(
            !verified
                .clone()
                .for_plugin_invoke(false, "admin", &[], &json!({}))
                .second_factor
        );
        assert!(
            !verified
                .clone()
                .for_plugin_invoke(false, "read", &[], &json!({}))
                .second_factor
        );
        assert!(
            verified
                .for_plugin_invoke(false, "super-admin", &[], &json!({}))
                .second_factor
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn call_scopes_to_the_plugin_reserves_core_args_and_reaches_internal_ops() {
        let mut s = surface("callp", "callp.{endpoint}.token");
        s.ops[1].internal = true;
        register("callp", s).unwrap();
        let admin = caller("admin", false);
        let tool = execute(
            "callp",
            "callp.create",
            json!({"endpoint": "main", "name": "n"}),
            &reached(&admin),
        )
        .await;
        assert!(
            tool.unwrap_err()
                .to_string()
                .contains("no api-surface tool")
        );
        for reserved in ["endpoint", "execute", "dryRun"] {
            let mut args = json!({"name": "n"});
            args[reserved] = json!(true);
            let err = call(
                "callp",
                None,
                "callp.create",
                "main",
                args,
                &reached(&admin),
            )
            .await;
            assert!(
                err.unwrap_err().to_string().contains("set by core"),
                "{reserved}"
            );
        }
        assert!(
            call(
                "other",
                None,
                "callp.create",
                "main",
                json!({}),
                &reached(&admin)
            )
            .await
            .is_err()
        );
        assert!(
            call(
                "callp",
                Some("nope"),
                "callp.create",
                "main",
                json!({}),
                &reached(&admin)
            )
            .await
            .is_err()
        );
        let reader = call(
            "callp",
            None,
            "callp.create",
            "main",
            json!({"name": "n"}),
            &reached(&caller("read", true)),
        )
        .await;
        assert!(reader.unwrap_err().downcast_ref::<Forbidden>().is_some());
        deregister("callp");
    }

    #[test]
    fn strips_userinfo_and_scrubs_bodies() {
        assert_eq!(
            strip_userinfo("https://u:p@h:1/x?a=b"),
            "https://***@h:1/x?a=b"
        );
        assert_eq!(strip_userinfo("http://h/x@y"), "http://h/x@y");
        assert!(!scrub_body(b"Authorization: Bearer zzz9", None).contains("zzz9"));
        let echoed = format!(
            "k=a%2Fb%3Dc&b={}&u={}",
            utils::encoding::base64_encode(b"a/b=c"),
            utils::encoding::base64url_encode(b"a/b=c")
        );
        let out = scrub_body(echoed.as_bytes(), Some("a/b=c"));
        assert!(!out.contains("a%2Fb%3Dc"), "{out}");
        assert!(
            !out.contains(utils::encoding::base64_encode(b"a/b=c").trim_end_matches('=')),
            "{out}"
        );
        assert!(
            !out.contains(utils::encoding::base64url_encode(b"a/b=c").trim_end_matches('=')),
            "{out}"
        );
        assert_eq!(
            scrub_body(&vec![b'a'; 5000], None).chars().count(),
            MAX_ERROR_BODY + 1
        );
    }

    #[test]
    fn a_chained_ctx_without_a_budget_is_exhausted() {
        let admin = CallerIdentity {
            user_id: "u".into(),
            username: "u".into(),
            role: "admin".into(),
            can_mutate: false,
        };
        let mut chained = ctx(&admin, false, true, false);
        chained.set_call_chain(vec!["p".into()]);
        let inv = Invocation::from_ctx(&chained).unwrap();
        assert!(inv.call_budget().load(std::sync::atomic::Ordering::SeqCst) >= 64);
        let root = Invocation::from_ctx(&ctx(&admin, false, true, false)).unwrap();
        assert_eq!(
            root.call_budget().load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}
