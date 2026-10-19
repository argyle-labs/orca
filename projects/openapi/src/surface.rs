#![allow(clippy::disallowed_types)]

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utils::json_wire::{bool_ish, number_ish, push_query, wire_string};

pub const FORMAT: &str = "orca.api-surface/v1";

const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSurface {
    pub format: String,
    pub surface: String,
    pub kind: SurfaceKind,
    #[serde(default)]
    pub base: Base,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub response: ResponseRules,
    #[serde(default)]
    pub endpoint: EndpointBinding,
    #[serde(default)]
    pub defs: Map<String, Value>,
    pub ops: Vec<ApiOp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SurfaceKind {
    Openapi,
    Graphql,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Base {
    #[serde(default)]
    pub api_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointBinding {
    #[serde(default = "default_endpoint_table")]
    pub table: String,
    #[serde(default = "default_endpoint_key")]
    pub key: String,
    #[serde(default)]
    pub fields: EndpointFields,
}

impl Default for EndpointBinding {
    fn default() -> Self {
        Self {
            table: default_endpoint_table(),
            key: default_endpoint_key(),
            fields: EndpointFields::default(),
        }
    }
}

fn default_endpoint_table() -> String {
    "endpoints".to_string()
}

fn default_endpoint_key() -> String {
    "name".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointFields {
    #[serde(default = "default_routes_field")]
    pub routes: String,
    #[serde(default = "default_insecure_field")]
    pub insecure: String,
    #[serde(default = "default_enabled_field")]
    pub enabled: String,
}

impl Default for EndpointFields {
    fn default() -> Self {
        Self {
            routes: default_routes_field(),
            insecure: default_insecure_field(),
            enabled: default_enabled_field(),
        }
    }
}

fn default_routes_field() -> String {
    "routes".to_string()
}

fn default_insecure_field() -> String {
    "insecure".to_string()
}

fn default_enabled_field() -> String {
    "enabled".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthScheme {
    Header,
    Query,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub scheme: AuthScheme,
    pub header: String,
    #[serde(default = "default_template")]
    pub template: String,
    pub secret: String,
}

fn default_template() -> String {
    "{secret}".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_as: Option<Value>,
    #[serde(default)]
    pub lenient_bool: bool,
    #[serde(default)]
    pub lenient_number: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiOp {
    pub tool: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub params: Vec<Param>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<OpBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OpOutput>,
    pub policy: Policy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamIn {
    Path,
    Query,
    Header,
    Body,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub name: String,
    #[serde(rename = "in")]
    pub location: ParamIn,
    #[serde(default)]
    pub required: bool,
    #[serde(default = "open_schema")]
    pub schema: Value,
}

fn open_schema() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpBody {
    #[serde(default = "open_schema")]
    pub schema: Value,
    #[serde(default = "default_content_type")]
    pub content_type: String,
}

fn default_content_type() -> String {
    "application/json".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpOutput {
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub role: String,
    #[serde(default)]
    pub execute_gated: bool,
    #[serde(default)]
    pub data_mutation: bool,
    #[serde(default)]
    pub local_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ApiOp {
    pub fn input_schema(&self) -> Value {
        let properties: Map<String, Value> = self
            .params
            .iter()
            .map(|p| (p.name.clone(), p.schema.clone()))
            .collect();
        let required: Vec<Value> = self
            .params
            .iter()
            .filter(|p| p.required || p.location == ParamIn::Path)
            .map(|p| Value::String(p.name.clone()))
            .collect();
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }
}

impl ApiSurface {
    pub fn parse(text: &str) -> Result<Self> {
        let surface: Self = serde_json::from_str(text).context("api-surface: invalid document")?;
        if surface.format != FORMAT {
            bail!(
                "api-surface: unsupported format `{}` (want `{FORMAT}`)",
                surface.format
            );
        }
        Ok(surface)
    }

    pub fn find(&self, tool: &str) -> Option<&ApiOp> {
        self.ops.iter().find(|op| op.tool == tool)
    }

    pub fn validate_args(&self, op: &ApiOp, args: &Value) -> Result<()> {
        let defs = Value::Object(self.defs.clone());
        utils::json_validate::validate(args, &op.input_schema(), &defs)
            .map_err(|errs| anyhow!("{}: invalid arguments: {}", op.tool, errs.join("; ")))
    }

    pub fn build_request(
        &self,
        op: &ApiOp,
        base_url: &str,
        args: &Map<String, Value>,
        secret: Option<&str>,
    ) -> Result<BuiltRequest> {
        let root = if self.base.api_root.is_empty() {
            base_url.trim_end_matches('/').to_string()
        } else {
            utils::url::join(base_url, &self.base.api_root)
        };
        let mut query: Vec<(String, String)> = Vec::new();
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut fields = Map::new();

        let (method, path) = match self.kind {
            SurfaceKind::Openapi => {
                let mut path = op.path.clone();
                for p in &op.params {
                    let Some(v) = args.get(&p.name).filter(|v| !v.is_null()) else {
                        if p.required || p.location == ParamIn::Path {
                            bail!("{}: missing required `{}`", op.tool, p.name);
                        }
                        continue;
                    };
                    match p.location {
                        ParamIn::Path => {
                            let raw = wire_string(v);
                            if raw.is_empty() || raw == "." || raw == ".." {
                                bail!(
                                    "{}: path parameter `{}` may not be `{raw}`",
                                    op.tool,
                                    p.name
                                );
                            }
                            path = path.replace(
                                &format!("{{{}}}", p.name),
                                &utils::url::encode(&raw).replace('.', "%2E"),
                            );
                        }
                        ParamIn::Query => push_query(&mut query, &p.name, v),
                        ParamIn::Header => headers.push((p.name.clone(), wire_string(v))),
                        ParamIn::Body => {
                            fields.insert(p.name.clone(), v.clone());
                        }
                    }
                }
                if let Some(open) = path.find('{')
                    && path[open..].contains('}')
                {
                    bail!("{}: unfilled path template `{path}`", op.tool);
                }
                (op.method.to_ascii_uppercase(), path)
            }
            SurfaceKind::Graphql => {
                for p in &op.params {
                    match args.get(&p.name).filter(|v| !v.is_null()) {
                        Some(v) => {
                            fields.insert(p.name.clone(), v.clone());
                        }
                        None if p.required => {
                            bail!("{}: missing required `{}`", op.tool, p.name)
                        }
                        None => {}
                    }
                }
                let gql = self.base.graphql_path.as_deref().unwrap_or("/graphql");
                ("POST".to_string(), gql.to_string())
            }
        };

        if let Some(auth) = &self.auth {
            let secret = secret
                .ok_or_else(|| anyhow!("{}: surface requires secret `{}`", op.tool, auth.secret))?;
            let value = auth.template.replace("{secret}", secret);
            match auth.scheme {
                AuthScheme::Header => headers.push((auth.header.clone(), value)),
                AuthScheme::Query => query.push((auth.header.clone(), value)),
            }
        }

        let mut url = utils::url::join(&root, &path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&utils::url::query_string(&query));
        }

        let body = match self.kind {
            SurfaceKind::Graphql => {
                headers.push(("content-type".into(), "application/json".into()));
                serde_json::to_vec(&serde_json::json!({
                    "query": op.path,
                    "variables": Value::Object(fields),
                }))?
            }
            SurfaceKind::Openapi if fields.is_empty() => Vec::new(),
            SurfaceKind::Openapi => {
                let content_type = op
                    .body
                    .as_ref()
                    .map(|b| b.content_type.as_str())
                    .unwrap_or("application/json");
                headers.push(("content-type".into(), content_type.to_string()));
                if content_type == "application/x-www-form-urlencoded" {
                    let mut pairs = Vec::new();
                    for (k, v) in &fields {
                        push_query(&mut pairs, k, v);
                    }
                    utils::url::query_string(&pairs).into_bytes()
                } else {
                    serde_json::to_vec(&Value::Object(fields))?
                }
            }
        };

        Ok(BuiltRequest {
            method,
            url,
            headers,
            body,
        })
    }

    pub fn decode_response(&self, op: &ApiOp, body: &[u8]) -> Result<Value> {
        let parsed: Value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body)
                .map_err(|e| anyhow!("{}: response is not JSON: {e}", op.tool))?
        };
        let mut out = peel_envelope(&self.response, parsed);
        if let Some(schema) = op.output.as_ref().map(|o| &o.schema) {
            coerce_lenient(&mut out, schema, &self.defs, &self.response);
        }
        Ok(out)
    }
}

pub fn peel_envelope(rules: &ResponseRules, value: Value) -> Value {
    let inner = match (&rules.envelope, value) {
        (Some(key), Value::Object(mut map)) if map.contains_key(key) => {
            map.remove(key).unwrap_or(Value::Null)
        }
        (_, other) => other,
    };
    match (inner, &rules.null_as) {
        (Value::Null, Some(replacement)) => replacement.clone(),
        (v, _) => v,
    }
}

pub fn coerce_lenient(
    value: &mut Value,
    schema: &Value,
    defs: &Map<String, Value>,
    rules: &ResponseRules,
) {
    if rules.lenient_bool || rules.lenient_number {
        coerce_at(value, schema, defs, rules, 0);
    }
}

fn coerce_at(
    value: &mut Value,
    schema: &Value,
    defs: &Map<String, Value>,
    rules: &ResponseRules,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Value::Object(obj) = schema else { return };
    if let Some(Value::String(r)) = obj.get("$ref") {
        if let Some(target) = r.rsplit('/').next().and_then(|leaf| defs.get(leaf)) {
            coerce_at(value, target, defs, rules, depth + 1);
        }
        return;
    }
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for sub in all {
            coerce_at(value, sub, defs, rules, depth + 1);
        }
    }

    let types = schema_types(obj);
    if rules.lenient_bool
        && types.contains(&"boolean")
        && !value.is_boolean()
        && !value.is_null()
        && let Some(b) = bool_ish(value)
    {
        *value = Value::Bool(b);
        return;
    }
    if rules.lenient_number
        && value.is_string()
        && (types.contains(&"number") || types.contains(&"integer"))
        && !types.contains(&"string")
        && let Some(n) = number_value(value)
    {
        *value = n;
        return;
    }

    match value {
        Value::Object(map) => {
            let props = obj.get("properties").and_then(Value::as_object);
            let extra = obj.get("additionalProperties").filter(|s| s.is_object());
            for (k, child) in map.iter_mut() {
                if let Some(sub) = props.and_then(|p| p.get(k)).or(extra) {
                    coerce_at(child, sub, defs, rules, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                for it in items {
                    coerce_at(it, item_schema, defs, rules, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn schema_types(obj: &Map<String, Value>) -> Vec<&str> {
    match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn number_value(v: &Value) -> Option<Value> {
    if let Some(i) = v.as_str().and_then(|s| s.trim().parse::<i64>().ok()) {
        return Some(Value::from(i));
    }
    number_ish(v)
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proxmox() -> ApiSurface {
        ApiSurface::parse(
            &json!({
                "format": FORMAT,
                "surface": "proxmox",
                "kind": "openapi",
                "base": {"api_root": "/api2/json"},
                "auth": {
                    "scheme": "header",
                    "header": "Authorization",
                    "template": "PVEAPIToken={secret}",
                    "secret": "proxmox.token"
                },
                "response": {
                    "envelope": "data",
                    "null_as": {},
                    "lenient_bool": true,
                    "lenient_number": true
                },
                "defs": {
                    "VmStatus": {
                        "type": "object",
                        "properties": {
                            "template": {"type": "boolean"},
                            "running": {"type": "boolean"},
                            "cpu": {"type": "number"},
                            "maxmem": {"type": "integer"},
                            "name": {"type": "string"}
                        }
                    }
                },
                "ops": [
                    {
                        "tool": "proxmox.vm_status",
                        "method": "get",
                        "path": "/nodes/{node}/qemu/{vmid}/status/current",
                        "params": [
                            {"name": "node", "in": "path", "schema": {"type": "string"}},
                            {"name": "vmid", "in": "path", "schema": {"type": "integer"}},
                            {"name": "full", "in": "query", "schema": {"type": "boolean"}}
                        ],
                        "output": {"schema": {"$ref": "#/components/schemas/VmStatus"}},
                        "policy": {"role": "user"}
                    },
                    {
                        "tool": "proxmox.list_vms",
                        "method": "GET",
                        "path": "/nodes/{node}/qemu",
                        "params": [{"name": "node", "in": "path"}],
                        "output": {"schema": {"type": "array", "items": {"$ref": "#/$defs/VmStatus"}}},
                        "policy": {"role": "user"}
                    },
                    {
                        "tool": "proxmox.create_role",
                        "method": "POST",
                        "path": "/access/roles",
                        "params": [
                            {"name": "roleid", "in": "body", "required": true},
                            {"name": "privs", "in": "body"}
                        ],
                        "body": {"schema": {"type": "object"}, "content_type": "application/x-www-form-urlencoded"},
                        "output": {"schema": {"type": "object"}},
                        "policy": {"role": "admin", "execute_gated": true, "data_mutation": true}
                    }
                ]
            })
            .to_string(),
        )
        .unwrap()
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn parse_rejects_wrong_format_and_unknown_fields() {
        let bad = json!({"format": "x/v0", "surface": "s", "kind": "openapi", "ops": []});
        assert!(ApiSurface::parse(&bad.to_string()).is_err());
        let extra =
            json!({"format": FORMAT, "surface": "s", "kind": "openapi", "ops": [], "nope": 1});
        assert!(ApiSurface::parse(&extra.to_string()).is_err());
    }

    #[test]
    fn endpoint_binding_defaults_to_shared_table_and_parses_overrides() {
        assert_eq!(proxmox().endpoint, EndpointBinding::default());
        let s = ApiSurface::parse(
            &json!({
                "format": FORMAT, "surface": "s", "kind": "openapi", "ops": [],
                "endpoint": {"table": "hosts", "key": "id", "fields": {"routes": "addrs"}}
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(s.endpoint.table, "hosts");
        assert_eq!(s.endpoint.key, "id");
        assert_eq!(s.endpoint.fields.routes, "addrs");
        assert_eq!(s.endpoint.fields.insecure, "insecure");
        let bad = json!({
            "format": FORMAT, "surface": "s", "kind": "openapi", "ops": [],
            "endpoint": {"fields": {"bogus": "x"}}
        });
        assert!(ApiSurface::parse(&bad.to_string()).is_err());
    }

    #[test]
    fn parse_reads_policy_and_defaults() {
        let s = proxmox();
        let op = s.find("proxmox.create_role").unwrap();
        assert_eq!(op.policy.role, "admin");
        assert!(op.policy.execute_gated && op.policy.data_mutation && !op.policy.local_only);
        assert!(!s.find("proxmox.list_vms").unwrap().params[0].required);
        assert!(s.find("missing").is_none());
    }

    #[test]
    fn builds_path_query_and_auth() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        let req = s
            .build_request(
                op,
                "https://pve:8006/",
                &args(json!({"node": "pve 1", "vmid": 100, "full": true})),
                Some("root@pam!t=uuid"),
            )
            .unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "https://pve:8006/api2/json/nodes/pve%201/qemu/100/status/current?full=true"
        );
        assert_eq!(
            req.headers,
            vec![("Authorization".into(), "PVEAPIToken=root@pam!t=uuid".into())]
        );
        assert!(req.body.is_empty());
    }

    #[test]
    fn missing_path_param_or_secret_errors() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        assert!(
            s.build_request(op, "https://h", &args(json!({"node": "n"})), Some("x"))
                .is_err()
        );
        assert!(
            s.build_request(
                op,
                "https://h",
                &args(json!({"node": "n", "vmid": 1})),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn path_params_cannot_climb_segments() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        for (node, vmid) in [
            (json!(".."), json!(1)),
            (json!("."), json!(1)),
            (json!(""), json!(1)),
            (json!("n"), json!("..")),
            (json!(".."), json!("..")),
        ] {
            assert!(
                s.build_request(
                    op,
                    "https://h",
                    &args(json!({"node": node, "vmid": vmid})),
                    Some("x")
                )
                .is_err()
            );
        }
        let req = s
            .build_request(
                op,
                "https://h",
                &args(json!({"node": "../..", "vmid": "a.b"})),
                Some("x"),
            )
            .unwrap();
        assert_eq!(
            req.url,
            "https://h/api2/json/nodes/%2E%2E%2F%2E%2E/qemu/a%2Eb/status/current"
        );
    }

    #[test]
    fn builds_form_body() {
        let s = proxmox();
        let op = s.find("proxmox.create_role").unwrap();
        let req = s
            .build_request(
                op,
                "https://h",
                &args(json!({"roleid": "ops", "privs": "VM.Audit,VM.Monitor"})),
                Some("x"),
            )
            .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://h/api2/json/access/roles");
        assert!(req.headers.contains(&(
            "content-type".into(),
            "application/x-www-form-urlencoded".into()
        )));
        assert_eq!(
            String::from_utf8(req.body).unwrap(),
            "roleid=ops&privs=VM.Audit%2CVM.Monitor"
        );
    }

    #[test]
    fn builds_json_body_by_default() {
        let mut s = proxmox();
        s.auth = None;
        let mut op = s.find("proxmox.create_role").unwrap().clone();
        op.body = None;
        let req = s
            .build_request(&op, "https://h", &args(json!({"roleid": "ops"})), None)
            .unwrap();
        assert!(
            req.headers
                .contains(&("content-type".into(), "application/json".into()))
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&req.body).unwrap(),
            json!({"roleid": "ops"})
        );
    }

    #[test]
    fn builds_graphql_request() {
        let s = ApiSurface::parse(
            &json!({
                "format": FORMAT,
                "surface": "unraid",
                "kind": "graphql",
                "base": {"graphql_path": "/graphql"},
                "ops": [{
                    "tool": "unraid.array",
                    "method": "POST",
                    "path": "query($id: ID!) { array(id: $id) { state } }",
                    "params": [{"name": "id", "in": "body", "required": true}],
                    "policy": {"role": "user"}
                }]
            })
            .to_string(),
        )
        .unwrap();
        let op = s.find("unraid.array").unwrap();
        let req = s
            .build_request(op, "http://tower", &args(json!({"id": "a1"})), None)
            .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "http://tower/graphql");
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["variables"], json!({"id": "a1"}));
        assert_eq!(body["query"], json!(op.path));
    }

    #[test]
    fn decode_peels_envelope_and_coerces_by_schema() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        let body = json!({"data": {
            "template": 0,
            "running": "1",
            "cpu": "0.25",
            "maxmem": "1024",
            "name": "100"
        }});
        let out = s.decode_response(op, body.to_string().as_bytes()).unwrap();
        assert_eq!(
            out,
            json!({"template": false, "running": true, "cpu": 0.25, "maxmem": 1024, "name": "100"})
        );
    }

    #[test]
    fn decode_coerces_array_items() {
        let s = proxmox();
        let op = s.find("proxmox.list_vms").unwrap();
        let body = json!({"data": [{"template": 1}, {"template": 0, "cpu": "2"}]});
        let out = s.decode_response(op, body.to_string().as_bytes()).unwrap();
        assert_eq!(
            out,
            json!([{"template": true}, {"template": false, "cpu": 2}])
        );
    }

    #[test]
    fn decode_null_payload_becomes_null_as() {
        let s = proxmox();
        let op = s.find("proxmox.create_role").unwrap();
        assert_eq!(
            s.decode_response(op, br#"{"data": null}"#).unwrap(),
            json!({})
        );
    }

    #[test]
    fn peel_passes_through_non_envelope_bodies() {
        let rules = ResponseRules {
            envelope: Some("data".into()),
            ..Default::default()
        };
        assert_eq!(peel_envelope(&rules, json!({"x": 1})), json!({"x": 1}));
        assert_eq!(peel_envelope(&rules, json!([1])), json!([1]));
        assert_eq!(peel_envelope(&rules, Value::Null), Value::Null);
    }

    #[test]
    fn coercion_off_leaves_values_and_unconvertible_values_stay() {
        let schema = json!({"type": "object", "properties": {
            "b": {"type": "boolean"}, "n": {"type": "number"}
        }});
        let mut v = json!({"b": 1, "n": "3"});
        coerce_lenient(&mut v, &schema, &Map::new(), &ResponseRules::default());
        assert_eq!(v, json!({"b": 1, "n": "3"}));

        let rules = ResponseRules {
            lenient_bool: true,
            lenient_number: true,
            ..Default::default()
        };
        let mut v = json!({"b": "maybe", "n": "abc"});
        coerce_lenient(&mut v, &schema, &Map::new(), &rules);
        assert_eq!(v, json!({"b": "maybe", "n": "abc"}));
    }

    #[test]
    fn number_coercion_skips_string_unions() {
        let rules = ResponseRules {
            lenient_number: true,
            ..Default::default()
        };
        let mut v = json!("7");
        coerce_lenient(
            &mut v,
            &json!({"type": ["string", "number"]}),
            &Map::new(),
            &rules,
        );
        assert_eq!(v, json!("7"));
    }

    #[test]
    fn input_schema_lists_params_and_required() {
        let s = proxmox();
        let schema = s.find("proxmox.vm_status").unwrap().input_schema();
        assert_eq!(schema["required"], json!(["node", "vmid"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["full"], json!({"type": "boolean"}));
    }

    #[test]
    fn validate_args_accepts_well_formed() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        assert!(
            s.validate_args(op, &json!({"node": "pve", "vmid": 100, "full": true}))
                .is_ok()
        );
    }

    #[test]
    fn validate_args_rejects_unknown_keys() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        let err = s
            .validate_args(op, &json!({"node": "pve", "vmid": 100, "bogus": 1}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unexpected property `bogus`"), "got: {err}");
    }

    #[test]
    fn validate_args_rejects_unknown_keys_on_param_less_op() {
        let mut op = proxmox().find("proxmox.vm_status").unwrap().clone();
        op.params.clear();
        let s = proxmox();
        assert!(s.validate_args(&op, &json!({})).is_ok());
        assert!(s.validate_args(&op, &json!({"x": 1})).is_err());
    }

    #[test]
    fn validate_args_rejects_missing_required_and_wrong_type() {
        let s = proxmox();
        let op = s.find("proxmox.vm_status").unwrap();
        let err = s
            .validate_args(op, &json!({"vmid": "abc"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing required `node`"), "got: {err}");
        assert!(err.contains("$.vmid: expected type"), "got: {err}");
        assert!(s.validate_args(op, &json!("not-an-object")).is_err());
    }

    #[test]
    fn validate_args_resolves_refs_into_defs() {
        let s = ApiSurface::parse(
            &json!({
                "format": FORMAT,
                "surface": "s",
                "kind": "openapi",
                "defs": {"Id": {"type": "integer"}},
                "ops": [{
                    "tool": "s.get",
                    "method": "GET",
                    "path": "/x/{id}",
                    "params": [{"name": "id", "in": "path", "schema": {"$ref": "#/components/schemas/Id"}}],
                    "policy": {"role": "user"}
                }]
            })
            .to_string(),
        )
        .unwrap();
        let op = s.find("s.get").unwrap();
        assert!(s.validate_args(op, &json!({"id": 3})).is_ok());
        assert!(s.validate_args(op, &json!({"id": "3"})).is_err());
    }
}
