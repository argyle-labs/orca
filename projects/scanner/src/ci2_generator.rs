/// CI2 OpenAPI generator for rebuyengine.com/api.php.
/// Static analysis only — no PHP runtime required.
/// Merges with any existing api.json at the repo root.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn generate(repo_path: &Path) -> Result<Value> {
    let api_php = repo_path.join("application/controllers/api.php");
    let src = std::fs::read_to_string(&api_php)?;

    let endpoints = extract_endpoints(&src);

    // Merge with existing api.json if present
    let existing_path = repo_path.join("api.json");
    let existing = if existing_path.exists() {
        let raw = std::fs::read_to_string(&existing_path)?;
        serde_json::from_str::<Value>(&raw).ok()
    } else {
        None
    };

    let spec = build_spec(endpoints, existing);
    Ok(spec)
}

// ── Endpoint model ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Endpoint {
    path: String,
    methods: Vec<String>,
    params: Vec<Param>,
    auth: Auth,
    response_keys: Vec<String>,
    summary: String,
}

#[derive(Debug, Clone)]
struct Param {
    name: String,
    required: bool,
    #[allow(dead_code)]
    in_query: bool, // true = query, false = body
}

#[derive(Debug, Clone, PartialEq)]
enum Auth {
    Public,
    ApiKey,
    ShopifyJwt,
    #[allow(dead_code)]
    RebuyPro,
}

// ── Endpoint extraction ───────────────────────────────────────────────────────

fn extract_endpoints(src: &str) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();

    // 1. Extract known private sub-dispatcher functions
    extract_v1_private_functions(src, &mut endpoints);

    // 2. Parse the v1() function for no-auth routes
    if let Some(body) = extract_function_body(src, "public function v1(") {
        extract_v1_no_auth_routes(&body, &mut endpoints);
    }

    // 3. Parse v1_api_key_required() for authenticated v1 routes
    if let Some(body) = extract_function_body(src, "private function v1_api_key_required(") {
        extract_v1_key_routes(&body, &mut endpoints);
    }

    // 4. Parse top-level public dispatch methods
    for (fn_sig, base_path, auth) in public_dispatch_functions() {
        if let Some(body) = extract_function_body(src, fn_sig) {
            extract_method_dispatch_routes(&body, base_path, auth, &mut endpoints);
        }
    }

    // Deduplicate by path+method
    dedup_endpoints(endpoints)
}

/// Known private sub-dispatcher function specs — these have fixed paths.
fn extract_v1_private_functions(src: &str, out: &mut Vec<Endpoint>) {
    // v1_theme: /api/v1/theme/id/{id}
    if let Some(body) = extract_function_body(src, "private function v1_theme(") {
        let params = scan_params(&body);
        out.push(Endpoint {
            path: "/api/v1/theme/id/{id}".to_string(),
            methods: vec!["GET".to_string()],
            params,
            auth: Auth::ShopifyJwt,
            response_keys: vec!["data".to_string()],
            summary: "Get Shopify theme assets by theme ID".to_string(),
        });
    }

    // v1_discounts: /api/v1/discounts/code/{code}
    if let Some(body) = extract_function_body(src, "private function v1_discounts(") {
        let params = scan_params(&body);
        out.push(Endpoint {
            path: "/api/v1/discounts/code/{code}".to_string(),
            methods: vec!["GET".to_string()],
            params,
            auth: Auth::ShopifyJwt,
            response_keys: vec!["data".to_string()],
            summary: "Look up a discount code".to_string(),
        });
    }

    // v1_custom_metafield_namespaces: /api/v1/custom_metafield_namespaces
    if let Some(body) = extract_function_body(src, "private function v1_custom_metafield_namespaces(") {
        let methods = detect_http_methods(&body);
        let params = scan_params(&body);
        let resp = scan_response_keys(&body);
        out.push(Endpoint {
            path: "/api/v1/custom_metafield_namespaces".to_string(),
            methods,
            params,
            auth: Auth::ShopifyJwt,
            response_keys: resp,
            summary: "Manage custom metafield namespaces".to_string(),
        });
    }

    // v1_integration: /api/v1/integration/{integration}/{event}
    if let Some(_body) = extract_function_body(src, "private function v1_integration(") {
        out.push(Endpoint {
            path: "/api/v1/integration/{integration}/{event}".to_string(),
            methods: vec!["POST".to_string()],
            params: vec![],
            auth: Auth::ShopifyJwt,
            response_keys: vec!["data".to_string()],
            summary: "Post a marketing integration event".to_string(),
        });
    }
}

/// Public functions to parse, each with their base path and auth level.
fn public_dispatch_functions() -> Vec<(&'static str, &'static str, Auth)> {
    vec![
        ("public function widgets(", "/api/widgets", Auth::Public),
        ("public function smart_cart(", "/api/smart_cart", Auth::Public),
        ("public function post_purchase(", "/api/post_purchase", Auth::ShopifyJwt),
        ("public function draft_order(", "/api/draft_order", Auth::ShopifyJwt),
        ("public function promo(", "/api/promo", Auth::Public),
        ("public function reorder(", "/api/reorder", Auth::Public),
        ("public function products(", "/api/products", Auth::ApiKey),
        ("public function analytics(", "/api/analytics", Auth::ApiKey),
        ("public function recharge(", "/api/recharge", Auth::ApiKey),
        ("public function data_sources(", "/api/data_sources", Auth::ApiKey),
        ("public function user(", "/api/user", Auth::Public),
    ]
}

/// Parse v1() for routes that don't require an API key.
fn extract_v1_no_auth_routes(body: &str, out: &mut Vec<Endpoint>) {
    // Stop before the api key validation block
    let no_auth_section = if let Some(p) = body.find("process_api_key_request") {
        &body[..p]
    } else {
        body
    };

    // Routes explicitly dispatched before the key check
    let static_routes: &[(&str, &str, Vec<&str>, Auth, &str)] = &[
        ("widgets/settings",    "/api/v1/widgets/settings",  vec!["GET"], Auth::Public, "Widget settings by ID"),
        ("widgets/styles",      "/api/v1/widgets/styles",    vec!["GET"], Auth::Public, "Widget styles (CSS/JSON)"),
        ("widgets/templates",   "/api/v1/widgets/templates", vec!["GET"], Auth::Public, "Widget Liquid templates"),
        ("promo/settings",      "/api/v1/promo/settings",    vec!["GET"], Auth::Public, "Promo bar settings"),
        ("reorder/settings",    "/api/v1/reorder/settings",  vec!["GET"], Auth::Public, "Reorder landing page settings"),
        ("smart_cart/apps",     "/api/v1/smart_cart/apps",   vec!["GET"], Auth::Public, "Smart Cart app list"),
        ("shopify/post_purchase", "/api/v1/shopify/post_purchase/{method}", vec!["GET","POST"], Auth::ShopifyJwt, "Shopify post-purchase flow"),
        ("shopify/draft_order",   "/api/v1/shopify/draft_order/{method}",  vec!["GET","POST"], Auth::ShopifyJwt, "Draft order operations"),
    ];

    for (marker, path, methods, auth, summary) in static_routes {
        if no_auth_section.contains(marker) {
            out.push(Endpoint {
                path: path.to_string(),
                methods: methods.iter().map(|s| s.to_string()).collect(),
                params: vec![],
                auth: auth.clone(),
                response_keys: vec!["data".to_string()],
                summary: summary.to_string(),
            });
        }
    }

    // user/* routes listed in $valid_user_endpoints
    let user_sub = ["all", "shop", "stylesheet", "smart_cart", "smart_carts", "templates", "config"];
    for sub in &user_sub {
        if no_auth_section.contains(&format!("'{sub}'")) {
            out.push(Endpoint {
                path: format!("/api/v1/user/{sub}"),
                methods: vec!["GET".to_string()],
                params: vec![],
                auth: Auth::Public,
                response_keys: vec!["data".to_string()],
                summary: format!("User {sub} data"),
            });
        }
    }
}

/// Parse v1_api_key_required() for authenticated routes dispatched by arg1/arg2.
fn extract_v1_key_routes(body: &str, out: &mut Vec<Endpoint>) {
    // Walk the function extracting contiguous if/else-if blocks on $arg1
    let arg1_re = regex::Regex::new(r#"\(\$arg1\s*==\s*'([^']+)'"#).unwrap();
    let arg2_re = regex::Regex::new(r#"\(\$arg2\s*==\s*'([^']+)'"#).unwrap();

    // Collect all arg1 values with their approximate position
    let arg1_positions: Vec<(usize, String)> = arg1_re
        .captures_iter(body)
        .filter_map(|c| {
            let m = c.get(0)?;
            Some((m.start(), c[1].to_string()))
        })
        .collect();

    for (i, (pos, arg1)) in arg1_positions.iter().enumerate() {
        let end = arg1_positions.get(i + 1).map(|(p, _)| *p).unwrap_or(body.len());
        let block = &body[*pos..end];

        // Find arg2 values within this block
        let arg2_values: Vec<String> = arg2_re
            .captures_iter(block)
            .filter_map(|c| Some(c[1].to_string()))
            .collect();

        let params = scan_params(block);
        let methods = detect_http_methods(block);
        let resp_keys = scan_response_keys(block);
        let required_params = scan_required_params(block);

        // Merge required flag into params
        let mut all_params = params;
        for rp in &required_params {
            if !all_params.iter().any(|p| &p.name == rp) {
                all_params.push(Param { name: rp.clone(), required: true, in_query: true });
            } else {
                for p in all_params.iter_mut() {
                    if &p.name == rp { p.required = true; }
                }
            }
        }

        if arg2_values.is_empty() {
            out.push(Endpoint {
                path: format!("/api/v1/{arg1}"),
                methods: methods.clone(),
                params: all_params.clone(),
                auth: Auth::ApiKey,
                response_keys: resp_keys.clone(),
                summary: String::new(),
            });
        } else {
            for arg2 in &arg2_values {
                out.push(Endpoint {
                    path: format!("/api/v1/{arg1}/{arg2}"),
                    methods: methods.clone(),
                    params: all_params.clone(),
                    auth: Auth::ApiKey,
                    response_keys: resp_keys.clone(),
                    summary: String::new(),
                });
            }
        }
    }
}

/// Parse a public dispatch function like widgets(), products(), etc.
fn extract_method_dispatch_routes(body: &str, base: &str, auth: Auth, out: &mut Vec<Endpoint>) {
    let method_re = regex::Regex::new(r#"\$method[s]?\s*==\s*'([^']+)'"#).unwrap();
    let method_positions: Vec<(usize, String)> = method_re
        .captures_iter(body)
        .filter_map(|c| {
            let m = c.get(0)?;
            Some((m.start(), c[1].to_string()))
        })
        .collect();

    if method_positions.is_empty() {
        // No dispatch — entire function is one endpoint
        let params = scan_params(body);
        let methods = detect_http_methods(body);
        let resp = scan_response_keys(body);
        // Check if the function uses $id or $arg1
        let path = if body.contains("$id") || body.contains("/{id}") {
            format!("{base}/{{id}}")
        } else {
            base.to_string()
        };
        out.push(Endpoint {
            path,
            methods,
            params,
            auth,
            response_keys: resp,
            summary: String::new(),
        });
        return;
    }

    for (i, (pos, method_val)) in method_positions.iter().enumerate() {
        let end = method_positions.get(i + 1).map(|(p, _)| *p).unwrap_or(body.len());
        let block = &body[*pos..end];
        let params = scan_params(block);
        let methods = detect_http_methods(block);
        let resp = scan_response_keys(block);
        let req_params = scan_required_params(block);
        let mut all_params = params;
        for rp in &req_params {
            if !all_params.iter().any(|p| &p.name == rp) {
                all_params.push(Param { name: rp.clone(), required: true, in_query: true });
            } else {
                for p in all_params.iter_mut() {
                    if &p.name == rp { p.required = true; }
                }
            }
        }
        out.push(Endpoint {
            path: format!("{base}/{method_val}"),
            methods,
            params: all_params,
            auth: auth.clone(),
            response_keys: resp,
            summary: String::new(),
        });
    }
}

// ── Scanning helpers ──────────────────────────────────────────────────────────

fn scan_params(block: &str) -> Vec<Param> {
    let re = regex::Regex::new(
        r#"(?:\$_GET|\$_REQUEST|\$_POST)\s*\[\s*'([^']+)'"#
    ).unwrap();
    let mut seen = BTreeSet::new();
    let mut params = Vec::new();
    for cap in re.captures_iter(block) {
        let name = cap[1].to_string();
        if seen.insert(name.clone()) {
            params.push(Param { name, required: false, in_query: true });
        }
    }
    // Also catch: ->input->get('X') style without the parens variation
    let re2 = regex::Regex::new(r#"\$this->input->\w+\('([^']+)'\)"#).unwrap();
    for cap in re2.captures_iter(block) {
        let name = cap[1].to_string();
        if seen.insert(name.clone()) {
            params.push(Param { name, required: false, in_query: true });
        }
    }
    params
}

fn scan_required_params(block: &str) -> Vec<String> {
    // Pattern: if (!isset($_REQUEST['X'])) { ... $missing_args[] = 'X' }
    // or: if (empty($this->input->get('X')))
    let re = regex::Regex::new(
        r#"!isset\(\$_(?:REQUEST|GET|POST)\['([^']+)'\]\)|empty\(\$this->input->\w+\('([^']+)'\)"#
    ).unwrap();
    let mut required = Vec::new();
    for cap in re.captures_iter(block) {
        let name = cap.get(1).or(cap.get(2)).map(|m| m.as_str().to_string());
        if let Some(n) = name {
            if !required.contains(&n) {
                required.push(n);
            }
        }
    }
    required
}

fn scan_response_keys(block: &str) -> Vec<String> {
    // Look for json_encode(array('key' => ...)) — top-level array keys
    let re = regex::Regex::new(r#"json_encode\s*\(\s*(?:array\s*\(|\[)\s*'([^']+)'\s*=>"#).unwrap();
    let mut keys = BTreeSet::new();
    for cap in re.captures_iter(block) {
        keys.insert(cap[1].to_string());
    }
    // Also catch: json_encode(['key' => ...]) multi-key
    let re2 = regex::Regex::new(r#"'([^']+)'\s*=>"#).unwrap();
    // Only look inside json_encode blocks
    let json_re = regex::Regex::new(r#"json_encode\s*\(([^;]{0,500})\)"#).unwrap();
    for jcap in json_re.captures_iter(block) {
        for cap in re2.captures_iter(&jcap[1]) {
            let k = cap[1].to_string();
            if !k.starts_with('$') {
                keys.insert(k);
            }
        }
    }
    keys.into_iter().collect()
}

fn detect_http_methods(block: &str) -> Vec<String> {
    // Check for explicit HTTP method guards
    let has_get = block.contains("'GET'") || block.contains("\"GET\"");
    let has_post = block.contains("'POST'") || block.contains("\"POST\"");
    let has_put = block.contains("'PUT'") || block.contains("\"PUT\"");
    let has_delete = block.contains("'DELETE'") || block.contains("\"DELETE\"");
    let has_patch = block.contains("'PATCH'") || block.contains("\"PATCH\"");

    let mut methods = Vec::new();
    if has_get   { methods.push("GET".to_string()); }
    if has_post  { methods.push("POST".to_string()); }
    if has_put   { methods.push("PUT".to_string()); }
    if has_delete { methods.push("DELETE".to_string()); }
    if has_patch { methods.push("PATCH".to_string()); }

    if methods.is_empty() {
        // No explicit method check — infer from context
        let has_post_write = block.contains("$_POST") || block.contains("->input->post");
        if has_post_write {
            methods.push("GET".to_string());
            methods.push("POST".to_string());
        } else {
            methods.push("GET".to_string());
        }
    }
    methods
}

// ── Function body extraction ──────────────────────────────────────────────────

fn extract_function_body(src: &str, sig: &str) -> Option<String> {
    let pos = src.find(sig)?;
    let after = &src[pos..];
    // Find first `{`
    let brace_start = after.find('{')?;
    let content = &after[brace_start..];
    // Walk matching braces
    let inner = extract_balanced(content, '{', '}')?;
    Some(inner.to_string())
}

fn extract_balanced(src: &str, open: char, close: char) -> Option<&str> {
    let mut chars = src.char_indices();
    let (_, first) = chars.next()?;
    if first != open { return None; }
    let mut depth = 1usize;
    for (i, ch) in chars {
        if ch == open  { depth += 1; }
        else if ch == close {
            depth -= 1;
            if depth == 0 { return Some(&src[1..i]); }
        }
    }
    None
}

fn dedup_endpoints(endpoints: Vec<Endpoint>) -> Vec<Endpoint> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut result: Vec<Endpoint> = Vec::new();
    for ep in endpoints {
        if let Some(idx) = seen.get(&ep.path) {
            // Merge methods
            for m in &ep.methods {
                if !result[*idx].methods.contains(m) {
                    result[*idx].methods.push(m.clone());
                }
            }
        } else {
            seen.insert(ep.path.clone(), result.len());
            result.push(ep);
        }
    }
    result
}

// ── OpenAPI spec builder ──────────────────────────────────────────────────────

fn build_spec(endpoints: Vec<Endpoint>, existing: Option<Value>) -> Value {
    let existing_paths = existing
        .as_ref()
        .and_then(|v| v["paths"].as_object())
        .cloned()
        .unwrap_or_default();

    let mut paths: BTreeMap<String, Value> = BTreeMap::new();

    // Seed with existing paths first (they win on conflict)
    for (k, v) in &existing_paths {
        paths.insert(k.clone(), v.clone());
    }

    for ep in endpoints {
        // Skip if already documented in existing spec
        if existing_paths.contains_key(&ep.path) {
            continue;
        }

        let security = match ep.auth {
            Auth::Public => vec![],
            Auth::ApiKey => vec![json!({"apiKeyV1": []})],
            Auth::ShopifyJwt => vec![json!({"shopifyJwt": []})],
            Auth::RebuyPro => vec![json!({"rebuyPro": []})],
        };

        let parameters = build_parameters(&ep.path, &ep.params);
        let responses = build_responses(&ep.response_keys);
        let tag = path_tag(&ep.path);

        let mut operation = json!({
            "operationId": operation_id(&ep.methods.first().map(|s| s.as_str()).unwrap_or("get"), &ep.path),
            "summary": ep.summary,
            "tags": [tag],
            "parameters": parameters,
            "responses": responses,
        });
        if !security.is_empty() {
            operation["security"] = json!(security);
        }

        let path_entry = paths.entry(ep.path.clone()).or_insert_with(|| json!({}));
        for method in &ep.methods {
            path_entry[method.to_lowercase().as_str()] = operation.clone();
        }
    }

    // Merge info/servers/security schemes from existing if present
    let (title, desc, servers, schemes) = if let Some(ref ex) = existing {
        (
            ex["info"]["title"].as_str().unwrap_or("Rebuy Engine API").to_string(),
            ex["info"]["description"].as_str().unwrap_or("").to_string(),
            ex["servers"].clone(),
            ex["components"]["securitySchemes"].clone(),
        )
    } else {
        (
            "Rebuy Engine API".to_string(),
            "Public API v1 endpoints served by rebuyengine.com".to_string(),
            json!([{"url": "https://rebuyengine.com", "description": "Production"}]),
            json!({
                "apiKeyV1": {
                    "type": "apiKey",
                    "in": "query",
                    "name": "key",
                    "description": "Rebuy API key"
                },
                "shopifyJwt": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Shopify session token (JWT from Shopify App Bridge)"
                },
                "rebuyPro": {
                    "type": "apiKey",
                    "in": "query",
                    "name": "rebuypro_secret_key",
                    "description": "Rebuy Pro internal secret key"
                }
            }),
        )
    };

    let existing_tags: Vec<Value> = existing
        .as_ref()
        .and_then(|v| v["tags"].as_array())
        .cloned()
        .unwrap_or_default();

    // Collect tags from generated paths
    let mut tag_set: BTreeSet<String> = existing_tags
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    for item in paths.values() {
        if let Some(obj) = item.as_object() {
            for op in obj.values() {
                if let Some(arr) = op["tags"].as_array() {
                    for t in arr {
                        if let Some(s) = t.as_str() { tag_set.insert(s.to_string()); }
                    }
                }
            }
        }
    }
    let tags: Vec<Value> = tag_set.into_iter().map(|t| json!({"name": t})).collect();

    let mut spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": title,
            "description": desc,
            "version": "1.0.0"
        },
        "servers": servers,
        "tags": tags,
        "paths": paths_to_value(paths),
        "components": {
            "securitySchemes": schemes
        }
    });

    // Carry over components/schemas from existing
    if let Some(ref ex) = existing {
        if let Some(schemas) = ex["components"]["schemas"].as_object() {
            spec["components"]["schemas"] = Value::Object(schemas.clone());
        }
    }

    spec
}

fn build_parameters(path: &str, params: &[Param]) -> Value {
    let mut out: Vec<Value> = Vec::new();

    // Path params from {name} placeholders
    let re = regex::Regex::new(r"\{([^}]+)\}").unwrap();
    for cap in re.captures_iter(path) {
        out.push(json!({
            "name": &cap[1],
            "in": "path",
            "required": true,
            "schema": { "type": "string" }
        }));
    }

    // Query params
    for p in params {
        // Skip params already covered as path params
        if re.captures_iter(path).any(|c| c[1] == p.name) { continue; }
        // Skip internal non-query-looking names
        if p.name == "bust_cache" || p.name == "rebuild_cache" { continue; }
        out.push(json!({
            "name": p.name,
            "in": "query",
            "required": p.required,
            "schema": { "type": "string" }
        }));
    }

    json!(out)
}

fn build_responses(keys: &[String]) -> Value {
    let schema = if keys.is_empty() {
        json!({ "type": "object" })
    } else {
        let props: serde_json::Map<String, Value> = keys
            .iter()
            .map(|k| (k.clone(), json!({})))
            .collect();
        json!({ "type": "object", "properties": props })
    };

    json!({
        "200": {
            "description": "Success",
            "content": {
                "application/json": { "schema": schema }
            }
        },
        "400": { "description": "Bad request / invalid parameters" },
        "401": { "description": "Unauthorized" },
        "404": { "description": "Not found" }
    })
}

fn path_tag(path: &str) -> String {
    // /api/v1/products/recommended → Products
    // /api/widgets/settings → Widgets
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let segment = match parts.as_slice() {
        [_, "v1", seg, ..] => *seg,
        [_, seg, ..] => *seg,
        _ => "misc",
    };
    let mut chars = segment.chars();
    match chars.next() {
        None => segment.to_string(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn operation_id(method: &str, path: &str) -> String {
    let cleaned = path
        .replace("{id}", "ById")
        .replace("{code}", "ByCode")
        .replace("{method}", "")
        .replace("{integration}", "ByIntegration")
        .replace("{event}", "ByEvent")
        .replace(['{', '}'], "");

    let parts: Vec<String> = cleaned
        .split('/')
        .filter(|s| !s.is_empty() && *s != "api")
        .enumerate()
        .map(|(i, s)| {
            if i == 0 { s.to_string() }
            else {
                let mut chars = s.chars();
                match chars.next() {
                    None => String::new(),
                    Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                }
            }
        })
        .collect();
    let suffix = parts.join("");
    let mut m = method.chars();
    let mc = match m.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + m.as_str(),
    };
    format!("{mc}{suffix}")
}

fn paths_to_value(paths: BTreeMap<String, Value>) -> Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in paths {
        obj.insert(k, v);
    }
    Value::Object(obj)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_methods_explicit_post() {
        assert!(detect_http_methods("if ('POST' == $m)").contains(&"POST".to_string()));
    }

    #[test]
    fn detect_methods_default_get() {
        assert_eq!(detect_http_methods("$x = 1;"), vec!["GET"]);
    }

    #[test]
    fn scan_params_get() {
        let block = "return $_GET['shop_id']; return $_REQUEST['limit'];";
        let params = scan_params(block);
        let names: Vec<_> = params.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"shop_id"));
        assert!(names.contains(&"limit"));
    }

    #[test]
    fn path_tag_v1() {
        assert_eq!(path_tag("/api/v1/products/recommended"), "Products");
    }

    #[test]
    fn path_tag_top() {
        assert_eq!(path_tag("/api/widgets/settings"), "Widgets");
    }
}
