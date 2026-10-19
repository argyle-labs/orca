#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use contract::CallerIdentity;
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

#[derive(Debug, Clone, Copy)]
pub struct Invocation<'a> {
    pub caller: &'a CallerIdentity,
    pub dry_run: bool,
    pub execute_confirmed: bool,
    pub local: bool,
    pub second_factor: bool,
}

pub fn register(plugin: &str, surface: ApiSurface) -> Result<()> {
    db::plugin_tables::validate_segment("plugin", plugin)?;
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
    let reg = SURFACES
        .read()
        .map_err(|_| anyhow!("surface registry poisoned"))?;
    reg.get(plugin)
        .into_iter()
        .flat_map(|s| s.values())
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

fn gate(surface: &ApiSurface, op: &ApiOp, inv: &Invocation<'_>) -> Result<()> {
    let required = required_role(surface.kind, op);
    let deny = |reason: String| -> Result<()> {
        Err(Forbidden {
            tool: op.tool.clone(),
            reason,
        }
        .into())
    };
    if !dispatch::tool_roles::satisfies(&inv.caller.role, required) {
        return deny(format!("requires role `{required}`"));
    }
    if op.policy.local_only && !inv.local {
        return deny("local callers only".into());
    }
    if required == "super-admin" && !inv.second_factor {
        return deny("requires a verified second factor".into());
    }
    if op.policy.execute_gated && !inv.dry_run && !inv.execute_confirmed {
        return deny("requires explicit execute confirmation".into());
    }
    Ok(())
}

pub async fn execute(
    plugin: &str,
    tool: &str,
    args: Value,
    inv: &Invocation<'_>,
) -> Result<Outcome> {
    let (surface, index) = lookup(plugin, tool)?;
    let op = &surface.ops[index];
    gate(&surface, op, inv)?;

    let Value::Object(mut args) = args else {
        bail!("{tool}: arguments must be an object");
    };
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
        .with_context(|| format!("{tool}: endpoint `{endpoint}`"))?;
    let route_key = format!("{plugin}.{endpoint}");
    let secret_name = surface
        .auth
        .as_ref()
        .map(|auth| auth.secret.replace("{endpoint}", &endpoint));

    if inv.dry_run {
        if let Some(name) = &secret_name {
            let op = SecretOp::Exists { name: name.clone() };
            if !utils::reactor::off_async_worker(|| secret_op(plugin, &op))?.found {
                bail!("no secret named '{name}'");
            }
        }
        let base = utils::route::resolve_reachable(&route_key, &routes, |_| async { true }).await?;
        let secret = secret_name.as_ref().map(|_| REDACTED);
        let req = surface.build_request(op, &base, &args, secret)?;
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
    let req = surface.build_request(op, &base, &args, secret.as_deref())?;
    let builder = http_client()
        .raw_request(utils::http::RawRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
            timeout_ms: None,
            insecure,
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

fn http_client() -> &'static utils::http::Client {
    static CLIENT: OnceLock<utils::http::Client> = OnceLock::new();
    CLIENT.get_or_init(utils::http::Client::new)
}

fn endpoint_row(plugin: &str, binding: &EndpointBinding, endpoint: &str) -> Result<DbRow> {
    let shared = binding.table == SHARED_ENDPOINTS;
    let reply = db::plugin_tables::exec_db_op_pooled(&DbOp::Get {
        namespace: if shared {
            String::new()
        } else {
            plugin.to_string()
        },
        table: binding.table.clone(),
        key_col: binding.key.clone(),
        key: endpoint.to_string(),
    })?;
    reply
        .rows
        .into_iter()
        .find(|r| !shared || matches!(r.get("provider"), Some(DbValue::Text(p)) if p == plugin))
        .ok_or_else(|| anyhow!("plugin `{plugin}` has no endpoint `{endpoint}`"))
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

    fn inv(caller: &CallerIdentity, dry_run: bool) -> Invocation<'_> {
        Invocation {
            caller,
            dry_run,
            execute_confirmed: false,
            local: true,
            second_factor: false,
        }
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
        assert!(gate(&s, &gated, &inv(&admin, true)).is_ok());
        assert!(gate(&s, &gated, &inv(&admin, false)).is_err());
        let confirmed = Invocation {
            execute_confirmed: true,
            ..inv(&admin, false)
        };
        assert!(gate(&s, &gated, &confirmed).is_ok());

        let mut local = op("GET", "read", false);
        local.policy.local_only = true;
        let remote = Invocation {
            local: false,
            ..inv(&admin, true)
        };
        assert!(gate(&s, &local, &remote).is_err());
        assert!(gate(&s, &local, &inv(&admin, true)).is_ok());

        let sensitive = op("POST", "super-admin", false);
        let root = caller("super-admin", false);
        assert!(gate(&s, &sensitive, &inv(&root, true)).is_err());
        let verified = Invocation {
            second_factor: true,
            ..inv(&root, true)
        };
        assert!(gate(&s, &sensitive, &verified).is_ok());
        assert!(
            gate(
                &s,
                &sensitive,
                &Invocation {
                    caller: &admin,
                    ..verified
                }
            )
            .is_err()
        );
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
}
