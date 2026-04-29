/// CI4 OpenAPI generator — Option B: parse route files + JSON schemas directly,
/// no changes to admin-api source required.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn generate(repo_path: &Path) -> Result<Value> {
    let routes_root = repo_path.join("apps/ci4/app/Config/Routes");
    let schemas_root = repo_path.join("apps/ci4/app/Schemas");

    let routes = collect_routes(&routes_root)?;

    let mut paths: BTreeMap<String, Value> = BTreeMap::new();
    let mut components_schemas: BTreeMap<String, Value> = BTreeMap::new();
    let mut schema_name_map: BTreeMap<String, String> = BTreeMap::new(); // filter path → component name

    for route in &routes {
        let oas_path = ci4_path_to_oas(&route.path);
        let params = path_params(&route.path);

        // Resolve json-schema filter if present
        let mut request_body: Option<Value> = None;
        for filter in &route.filters {
            if let Some(schema_path) = filter.strip_prefix("json-schema:") {
                let trimmed = schema_path.trim_start_matches('/');
                let file_path = schemas_root.join(trimmed);
                if let Ok(raw) = std::fs::read_to_string(&file_path) {
                    if let Ok(schema) = serde_json::from_str::<Value>(&raw) {
                        let schema = clean_json_schema(schema);
                        let cname = schema_component_name(trimmed);
                        components_schemas.insert(cname.clone(), schema);
                        schema_name_map.insert(trimmed.to_string(), cname.clone());
                        request_body = Some(json!({
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": format!("#/components/schemas/{cname}") }
                                }
                            }
                        }));
                    }
                }
                break;
            }
        }

        let security = auth_security(&route.filters);
        let tag = path_tag(&route.path);

        let mut operation = json!({
            "operationId": operation_id(&route.method, &route.path),
            "summary": "",
            "tags": [tag],
            "parameters": params,
            "responses": standard_responses(),
        });

        if let Some(rb) = request_body {
            operation["requestBody"] = rb;
        }
        if !security.is_empty() {
            operation["security"] = json!(security);
        }

        let path_entry = paths.entry(oas_path).or_insert_with(|| json!({}));
        path_entry[route.method.as_str()] = operation;
    }

    // Inject standard envelope schemas
    components_schemas.insert(
        "ApiResponse".to_string(),
        json!({
            "type": "object",
            "properties": {
                "successful": { "type": "boolean" },
                "data": {},
                "status":    { "type": "integer" },
                "errorMessage": { "type": ["string", "null"] }
            }
        }),
    );

    let spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "admin-api",
            "version": "0.0.0",
            "description": "Rebuy admin API — auto-generated from CI4 route files and JSON schemas"
        },
        "servers": [
            { "url": "https://api.rebuyengine.com", "description": "Production" }
        ],
        "security": [],
        "tags": build_tags(&paths),
        "paths": paths_to_value(paths),
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                },
                "shopAuth": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "X-Shopify-Shop-Domain"
                }
            },
            "schemas": schemas_to_value(components_schemas)
        }
    });

    Ok(spec)
}

// ── Route collection ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct Ci4Route {
    method: String,
    path: String,
    filters: Vec<String>,
}

fn collect_routes(routes_root: &Path) -> Result<Vec<Ci4Route>> {
    let mut all = Vec::new();
    if !routes_root.exists() {
        return Ok(all);
    }
    visit_dir(routes_root, &mut all)?;
    Ok(all)
}

fn visit_dir(dir: &Path, out: &mut Vec<Ci4Route>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            visit_dir(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("php") {
            let src = std::fs::read_to_string(&path)?;
            parse_routes(&src, out);
        }
    }
    Ok(())
}

/// Extract all `$routes->METHOD(...)` calls from a PHP file.
fn parse_routes(src: &str, out: &mut Vec<Ci4Route>) {
    let methods = ["get", "post", "put", "delete", "patch", "options", "head"];
    let mut pos = 0;
    let bytes = src.as_bytes();

    while pos < src.len() {
        // Find `$routes->`
        if let Some(rel) = src[pos..].find("$routes->") {
            pos += rel + "$routes->".len();

            // Check method name
            let rest = &src[pos..];
            let mut found_method = None;
            for m in &methods {
                if rest.starts_with(m) {
                    let after = rest[m.len()..].trim_start();
                    if after.starts_with('(') {
                        found_method = Some(*m);
                        break;
                    }
                }
            }
            let method = match found_method {
                Some(m) => m,
                None => continue,
            };
            pos += method.len();

            // Scan forward to collect everything inside the outer parens
            let call = match extract_balanced(&src[pos..], '(', ')') {
                Some(c) => c,
                None => continue,
            };
            pos += call.len() + 2; // +2 for the ( and )

            // Parse method call arguments
            if let Some(route) = parse_route_call(method, &call) {
                out.push(route);
            }
        } else {
            break;
        }
    }
}

/// Extract balanced delimiters content (not including the delimiters).
fn extract_balanced(src: &str, open: char, close: char) -> Option<&str> {
    let mut chars = src.char_indices();
    let (_, first) = chars.next()?;
    if first != open {
        return None;
    }
    let mut depth = 1usize;
    for (i, ch) in chars {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(&src[1..i]);
            }
        }
    }
    None
}

fn parse_route_call(method: &str, call: &str) -> Option<Ci4Route> {
    // call is the content inside the outer parens, e.g.:
    //   'admin/api/v1/shop', 'Api\V1\Shop::getShopObject', ['filter' => ['auth']]
    let path = extract_first_quoted(call)?;

    // Extract filter array: look for 'filter' => [...]
    let filters = extract_filters(call);

    Some(Ci4Route {
        method: method.to_string(),
        path,
        filters,
    })
}

fn extract_first_quoted(src: &str) -> Option<String> {
    // Find first ' or "
    let mut chars = src.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch == '\'' || ch == '"' {
            let quote = ch;
            let mut result = String::new();
            let mut escaped = false;
            for (_, c) in chars.by_ref() {
                if escaped {
                    result.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    return Some(result);
                } else {
                    result.push(c);
                }
            }
            let _ = i; // suppress warning
            break;
        }
    }
    None
}

fn extract_filters(src: &str) -> Vec<String> {
    // Find 'filter' => [...] and extract the quoted strings inside
    let mut filters = Vec::new();

    // Find the filter array bracket
    let filter_key = "'filter'";
    let Some(fpos) = src.find(filter_key) else {
        return filters;
    };
    let after = &src[fpos + filter_key.len()..];

    // Skip whitespace and `=>`
    let trimmed = after.trim_start().trim_start_matches("=>").trim_start();

    // Find balanced [...]
    let Some(inner) = extract_balanced(trimmed, '[', ']') else {
        return filters;
    };

    // Extract all quoted strings from inner
    let mut pos = 0;
    let bytes = inner.as_bytes();
    while pos < inner.len() {
        if inner.as_bytes()[pos] == b'\'' || bytes[pos] == b'"' {
            let quote = inner.as_bytes()[pos] as char;
            pos += 1;
            let mut val = String::new();
            let mut escaped = false;
            while pos < inner.len() {
                let c = inner.as_bytes()[pos] as char;
                pos += 1;
                if escaped {
                    val.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    break;
                } else {
                    val.push(c);
                }
            }
            if !val.is_empty() {
                filters.push(val);
            }
        } else {
            pos += 1;
        }
    }

    filters
}

// ── Path conversion ───────────────────────────────────────────────────────────

/// Convert CI4 path to OAS path. `(:num)` → `{id}`, `(:segment)` → `{segment}`, etc.
fn ci4_path_to_oas(path: &str) -> String {
    let oas = format!("/{path}");
    // Replace (:num), (:segment), (:any), (:alpha) with named params
    let oas = replace_path_params(&oas);
    oas
}

fn replace_path_params(path: &str) -> String {
    let mut result = String::new();
    let mut counters: BTreeMap<String, u32> = BTreeMap::new();
    let mut remaining = path;

    while let Some(start) = remaining.find("(:") {
        result.push_str(&remaining[..start]);
        let after = &remaining[start + 2..]; // skip "(:"
        if let Some(end) = after.find(')') {
            let kind = &after[..end];
            let base_name = match kind {
                "num" => "id",
                "segment" => "segment",
                "any" => "param",
                "alpha" => "slug",
                _ => "param",
            };
            let count = counters.entry(base_name.to_string()).or_insert(0);
            *count += 1;
            let name = if *count == 1 {
                // Leave as-is; if there ends up being only one, the name is clean
                base_name.to_string()
            } else {
                format!("{base_name}{count}")
            };
            result.push('{');
            result.push_str(&name);
            result.push('}');
            remaining = &after[end + 1..];
        } else {
            result.push_str("(:"); // malformed, pass through
            remaining = after;
        }
    }
    result.push_str(remaining);

    // Fix: if there's only one of a kind but we used "id1", we need a second pass
    // Instead, let's use a simpler approach that's more accurate
    result
}

fn path_params(ci4_path: &str) -> Value {
    let mut params = Vec::new();
    let mut remaining = ci4_path;
    let mut counters: BTreeMap<String, u32> = BTreeMap::new();

    while let Some(start) = remaining.find("(:") {
        let after = &remaining[start + 2..];
        if let Some(end) = after.find(')') {
            let kind = &after[..end];
            let (base_name, schema_type) = match kind {
                "num" => ("id", "integer"),
                "segment" => ("segment", "string"),
                "any" => ("param", "string"),
                "alpha" => ("slug", "string"),
                _ => ("param", "string"),
            };
            let count = counters.entry(base_name.to_string()).or_insert(0);
            *count += 1;
            let name = if *count > 1 {
                format!("{base_name}{count}")
            } else {
                base_name.to_string()
            };
            params.push(json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": schema_type }
            }));
            remaining = &after[end + 1..];
        } else {
            break;
        }
    }

    json!(params)
}

// ── Auth → security ───────────────────────────────────────────────────────────

fn auth_security(filters: &[String]) -> Vec<Value> {
    let has_auth = filters.iter().any(|f| {
        f.starts_with("auth") || f == "shop-admin" || f == "rebuy-admin" || f == "auth-clt"
    });
    if has_auth {
        vec![json!({ "bearerAuth": [] })]
    } else {
        vec![]
    }
}

// ── Responses ─────────────────────────────────────────────────────────────────

fn standard_responses() -> Value {
    json!({
        "200": {
            "description": "Success",
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/ApiResponse" }
                }
            }
        },
        "401": { "description": "Unauthorized" },
        "403": { "description": "Forbidden" },
        "404": { "description": "Not found" },
        "422": { "description": "Validation error" },
        "500": { "description": "Server error" }
    })
}

// ── Tags ──────────────────────────────────────────────────────────────────────

fn path_tag(ci4_path: &str) -> String {
    // Use the first meaningful path segment after "admin/api/v1/" or "admin/api/v2/"
    for prefix in &["admin/api/v1/", "admin/api/v2/", "admin/api/"] {
        if let Some(rest) = ci4_path.strip_prefix(prefix) {
            let segment = rest.split('/').next().unwrap_or("misc");
            return segment.replace('-', "_");
        }
    }
    // Fallback: second segment
    let parts: Vec<&str> = ci4_path.split('/').collect();
    parts.get(1).unwrap_or(&"misc").to_string()
}

fn build_tags(paths: &BTreeMap<String, Value>) -> Value {
    let mut tags: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for item in paths.values() {
        if let Some(obj) = item.as_object() {
            for op in obj.values() {
                if let Some(arr) = op["tags"].as_array() {
                    for t in arr {
                        if let Some(s) = t.as_str() {
                            tags.insert(s.to_string());
                        }
                    }
                }
            }
        }
    }
    Value::Array(
        tags.into_iter()
            .map(|t| json!({ "name": t }))
            .collect(),
    )
}

// ── operationId ───────────────────────────────────────────────────────────────

fn operation_id(method: &str, ci4_path: &str) -> String {
    // e.g. get admin/api/v1/shop/(:num) → getAdminApiV1ShopId
    let cleaned = ci4_path
        .replace("(:num)", "ById")
        .replace("(:segment)", "BySegment")
        .replace("(:any)", "ByParam")
        .replace("(:alpha)", "BySlug");

    let parts: Vec<String> = cleaned
        .split('/')
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, s)| {
            if i == 0 {
                s.to_string()
            } else {
                let mut chars = s.chars();
                match chars.next() {
                    None => String::new(),
                    Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                }
            }
        })
        .collect();

    let suffix = parts.join("");
    let mut chars = method.chars();
    let method_cap = match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    };
    format!("{method_cap}{suffix}")
}

// ── JSON schema cleaning ──────────────────────────────────────────────────────

fn clean_json_schema(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        // Remove CI4-specific $pragma keys recursively
        clean_pragmas(obj);
    }
    v
}

fn clean_pragmas(obj: &mut serde_json::Map<String, Value>) {
    obj.remove("$pragma");
    for val in obj.values_mut() {
        if let Some(child) = val.as_object_mut() {
            clean_pragmas(child);
        } else if let Some(arr) = val.as_array_mut() {
            for item in arr.iter_mut() {
                if let Some(child) = item.as_object_mut() {
                    clean_pragmas(child);
                }
            }
        }
    }
}

fn schema_component_name(filter_path: &str) -> String {
    // "V1/Shop/PutTheme.json" → "V1ShopPutTheme"
    filter_path
        .trim_end_matches(".json")
        .replace(['/', '\\'], "")
        .replace('-', "_")
}

// ── Value helpers ─────────────────────────────────────────────────────────────

fn paths_to_value(paths: BTreeMap<String, Value>) -> Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in paths {
        obj.insert(k, v);
    }
    Value::Object(obj)
}

fn schemas_to_value(schemas: BTreeMap<String, Value>) -> Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in schemas {
        obj.insert(k, v);
    }
    Value::Object(obj)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci4_path_num_param() {
        assert_eq!(ci4_path_to_oas("admin/api/v1/shop/(:num)"), "/admin/api/v1/shop/{id}");
    }

    #[test]
    fn ci4_path_segment_param() {
        assert_eq!(
            ci4_path_to_oas("admin/api/v1/shop/theme/(:segment)"),
            "/admin/api/v1/shop/theme/{segment}"
        );
    }

    #[test]
    fn operation_id_simple() {
        assert_eq!(
            operation_id("get", "admin/api/v1/shop"),
            "GetadminApiV1Shop"
        );
    }

    #[test]
    fn schema_component_name_nested() {
        assert_eq!(schema_component_name("V1/Shop/PutTheme.json"), "V1ShopPutTheme");
    }

    #[test]
    fn path_tag_v1() {
        assert_eq!(path_tag("admin/api/v1/shop/theme"), "shop");
    }

    #[test]
    fn extract_filters_basic() {
        let call = r#"'admin/api/v1/shop', 'Shop::put', ['filter' => ['auth', 'json-schema:/V1/Shop/PutTheme.json']]"#;
        let filters = extract_filters(call);
        assert_eq!(filters, vec!["auth", "json-schema:/V1/Shop/PutTheme.json"]);
    }

    #[test]
    fn parse_routes_basic() {
        let src = r#"
$routes->get('admin/api/v1/shop', 'Shop::get', ['filter' => ['auth']]);
$routes->put('admin/api/v1/shop/(:num)', 'Shop::put/$1', ['filter' => ['auth', 'csrf']]);
"#;
        let mut routes = Vec::new();
        parse_routes(src, &mut routes);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].method, "get");
        assert_eq!(routes[0].path, "admin/api/v1/shop");
        assert_eq!(routes[1].method, "put");
        assert!(routes[1].path.contains("(:num)"));
    }
}
