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
    pub required: &'static str,
}

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: requires role `{}`", self.tool, self.required)
    }
}

impl std::error::Error for Forbidden {}

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

pub fn effective_policy(kind: SurfaceKind, op: &ApiOp) -> (&'static str, bool) {
    let declared = canonical_role(&op.policy.role);
    if kind == SurfaceKind::Openapi && op.method.eq_ignore_ascii_case("GET") {
        return (declared.unwrap_or("admin"), false);
    }
    let role = match declared {
        Some("super-admin") => "super-admin",
        _ => "admin",
    };
    (role, op.policy.data_mutation && role == "admin")
}

pub async fn execute(
    plugin: &str,
    tool: &str,
    args: Value,
    caller: &CallerIdentity,
    dry_run: bool,
) -> Result<Outcome> {
    let (surface, index) = lookup(plugin, tool)?;
    let op = &surface.ops[index];

    let (required, data_mutation) = effective_policy(surface.kind, op);
    if !dispatch::tool_roles::authorize(&caller.role, caller.can_mutate, required, data_mutation) {
        return Err(Forbidden {
            tool: op.tool.clone(),
            required,
        }
        .into());
    }

    let Value::Object(mut args) = args else {
        bail!("{tool}: arguments must be an object");
    };
    let endpoint = match args.remove(ENDPOINT_ARG) {
        Some(Value::String(e)) if !e.is_empty() => e,
        _ => bail!("{tool}: missing `{ENDPOINT_ARG}`"),
    };
    surface.validate_args(op, &Value::Object(args.clone()))?;

    let row = endpoint_row(plugin, &surface.endpoint, &endpoint)?;
    let fields = &surface.endpoint.fields;
    if row.get(&fields.enabled).is_some_and(|v| !truthy(v)) {
        bail!("{tool}: endpoint `{endpoint}` is disabled");
    }
    let insecure = row.get(&fields.insecure).is_some_and(truthy);
    let routes = routes_of(&row, &fields.routes)
        .with_context(|| format!("{tool}: endpoint `{endpoint}`"))?;
    let route_key = format!("{plugin}.{endpoint}");

    if dry_run {
        let base = utils::route::resolve_reachable(&route_key, &routes, |_| async { true }).await?;
        let secret = surface.auth.as_ref().map(|_| REDACTED);
        let req = surface.build_request(op, &base, &args, secret)?;
        return Ok(Outcome::Plan(plan(req, endpoint, &base)));
    }

    let secret = match &surface.auth {
        Some(auth) => Some(read_secret(
            plugin,
            &auth.secret.replace("{endpoint}", &endpoint),
        )?),
        None => None,
    };
    let base = plugin_toolkit::route::resolve_reachable(&route_key, &routes, insecure).await?;
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

fn read_secret(plugin: &str, name: &str) -> Result<String> {
    secrets::exec_secret_op_pooled(
        &secrets::Principal::Plugin(plugin.to_string()),
        &SecretOp::Get {
            name: name.to_string(),
        },
    )?
    .value
    .ok_or_else(|| anyhow!("no secret named '{name}'"))
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
        text = text.replace(s, REDACTED);
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
        assert_eq!(
            effective_policy(k, &op("GET", "read", false)),
            ("read", false)
        );
        assert_eq!(
            effective_policy(k, &op("get", "user", false)),
            ("user", false)
        );
        assert_eq!(
            effective_policy(k, &op("GET", "any", false)),
            ("admin", false)
        );
        assert_eq!(effective_policy(k, &op("GET", "", false)), ("admin", false));
        assert_eq!(
            effective_policy(k, &op("GET", "read", true)),
            ("read", false)
        );
        assert_eq!(
            effective_policy(k, &op("POST", "read", false)),
            ("admin", false)
        );
        assert_eq!(
            effective_policy(k, &op("DELETE", "any", true)),
            ("admin", true)
        );
        assert_eq!(
            effective_policy(k, &op("PUT", "super-admin", true)),
            ("super-admin", false)
        );
        assert_eq!(
            effective_policy(SurfaceKind::Graphql, &op("POST", "read", false)),
            ("admin", false)
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

    #[tokio::test]
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
                &caller("read", false),
                false,
            )
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, Outcome::Response(json!({"id": 5})));
        deregister("e2e");
    }

    #[tokio::test]
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
                &caller("admin", false),
                false,
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

    #[tokio::test]
    async fn dry_run_plans_without_sending() {
        let server = MockServer::start().await;
        register("dry", surface("dry", "dry.{endpoint}.token")).unwrap();
        let out = in_temp_db(async {
            seed("dry", &server, "s3cr3t");
            execute(
                "dry",
                "dry.create",
                json!({"endpoint": "main", "name": "n", "password": "pw"}),
                &caller("admin", false),
                true,
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

    #[tokio::test]
    async fn floor_denies_before_any_work() {
        let server = MockServer::start().await;
        register("deny", surface("deny", "deny.{endpoint}.token")).unwrap();
        let res = in_temp_db(async {
            seed("deny", &server, "s3cr3t");
            let write = execute(
                "deny",
                "deny.create",
                json!({"endpoint": "main", "name": "n"}),
                &caller("read", false),
                true,
            )
            .await;
            let mutate = execute(
                "deny",
                "deny.create",
                json!({"endpoint": "main", "name": "n"}),
                &caller("member", true),
                true,
            )
            .await;
            (write, mutate)
        })
        .await;
        assert!(res.0.unwrap_err().downcast_ref::<Forbidden>().is_some());
        assert!(matches!(res.1.unwrap(), Outcome::Plan(_)));
        assert!(server.received_requests().await.unwrap().is_empty());
        deregister("deny");
    }

    #[tokio::test]
    async fn foreign_secret_and_endpoint_are_refused() {
        let server = MockServer::start().await;
        register("own", surface("own", "other.{endpoint}.token")).unwrap();
        let res = in_temp_db(async {
            seed("own", &server, "s3cr3t");
            let secret = execute(
                "own",
                "own.item",
                json!({"endpoint": "main", "id": 1}),
                &caller("admin", false),
                false,
            )
            .await;
            let endpoint = execute(
                "own",
                "own.item",
                json!({"endpoint": "missing", "id": 1}),
                &caller("admin", false),
                false,
            )
            .await;
            (secret, endpoint)
        })
        .await;
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

    #[tokio::test]
    async fn rejects_missing_endpoint_and_unknown_args() {
        register("args", surface("args", "args.{endpoint}.token")).unwrap();
        let c = caller("admin", false);
        let missing = execute("args", "args.item", json!({"id": 1}), &c, true).await;
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
            &c,
            true,
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
        assert_eq!(
            scrub_body(&vec![b'a'; 5000], None).chars().count(),
            MAX_ERROR_BODY + 1
        );
    }
}
