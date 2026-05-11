//! CI4 (CodeIgniter 4) OpenAPI spec generator.
//!
//! Produces an OpenAPI 3.1.0 document by static analysis of the admin-api PHP
//! source — no PHP runtime required.
//!
//! ## Five-pass architecture
//!
//! The CI4 admin-api uses several distinct patterns for request validation and
//! response serialization.  A single-pass approach would miss most of them, so
//! the generator refines the spec incrementally:
//!
//! | Pass | Covers | Source |
//! |------|--------|--------|
//! | 1 | Route paths, HTTP methods, auth filters, JSON-Schema request bodies | `app/Config/Routes/*.php` + `app/Schemas/*.json` |
//! | 2 | Typed request bodies for write routes (Payload DTO or `getJSON` fields) | Controller source |
//! | 3 | Typed response wrappers declared with `#[ApiResponse(Class::class)]` | Controller attribute + response class source |
//! | 4 | Response schemas from `->setJSON([literal array])` return statements; engine-proxy routes marked `data: array` | Controller source (AST-free, string scan) |
//! | 5 | Response schemas via 3-hop trace: `$var->toArray()` → model `$returnType` → entity `$casts` | Controller + model + entity sources |
//!
//! Routes that survive all passes without a typed schema fall back to
//! `#/components/schemas/ApiResponse` — the generic envelope.
//!
//! ## AST-first, string fallback
//!
//! When the `php-ast` Cargo feature is enabled, each pass delegates to
//! [`crate::scanner::php_parse::PhpFile`] for accurate tree-sitter-based extraction.
//! When the feature is disabled (or `PhpFile::parse` returns `None` due to a
//! parse error), the string-scanning fallback functions below take over.
//! This means the scanner compiles and works in all configurations; the AST
//! path simply produces higher-fidelity results.
use crate::scanner::php_parse::PhpFile;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
                if let Ok(raw) = std::fs::read_to_string(&file_path)
                    && let Ok(schema) = serde_json::from_str::<Value>(&raw)
                {
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

    // Second pass: fill in requestBody for write routes that had no json-schema filter
    for route in &routes {
        let method = route.method.as_str();
        if !matches!(method, "post" | "put" | "patch") {
            continue;
        }
        let oas_path = ci4_path_to_oas(&route.path);
        // Skip if requestBody already set
        if let Some(path_item) = paths.get(&oas_path) {
            if path_item[method].get("requestBody").is_some() {
                continue;
            }
        } else {
            continue;
        }

        if route.controller.is_empty() {
            continue;
        }

        // Derive method name from controller ref (e.g. Api\V1\Foo::postCreate → postCreate)
        let method_name = route
            .controller
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();

        let schema = if let Some(ctrl_path) = resolve_controller_file(repo_path, &route.controller)
        {
            if let Ok(ctrl_src) = std::fs::read_to_string(&ctrl_path) {
                let mut found_schema: Option<Value> = None;

                // Try Payload class first
                if let Some(payload_class) = find_payload_class(&ctrl_src, &method_name)
                    && let Some(payload_path) =
                        resolve_payload_file(repo_path, &payload_class, &ctrl_src)
                    && let Ok(payload_src) = std::fs::read_to_string(&payload_path)
                {
                    let s = extract_payload_schema(&payload_src);
                    if s.get("properties")
                        .and_then(|p| p.as_object())
                        .map(|p| !p.is_empty())
                        .unwrap_or(false)
                    {
                        found_schema = Some(s);
                    }
                }

                // Fallback: getJSON field extraction
                if found_schema.is_none()
                    && let Some(body) = extract_method_body(&ctrl_src, &method_name)
                {
                    let s = extract_getjson_fields(body);
                    if s.get("properties")
                        .and_then(|p| p.as_object())
                        .map(|p| !p.is_empty())
                        .unwrap_or(false)
                    {
                        found_schema = Some(s);
                    }
                }

                found_schema
            } else {
                None
            }
        } else {
            None
        };

        if let Some(schema) = schema
            && let Some(path_item) = paths.get_mut(&oas_path)
        {
            path_item[method]["requestBody"] = json!({
                "required": true,
                "content": {
                    "application/json": {
                        "schema": schema
                    }
                }
            });
        }
    }

    // Third pass: override the 200 response for endpoints with #[ApiResponse(Class::class)].
    for route in &routes {
        let oas_path = ci4_path_to_oas(&route.path);
        if route.controller.is_empty() {
            continue;
        }
        let method_name = route
            .controller
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();

        let resolved = resolve_controller_file(repo_path, &route.controller)
            .and_then(|ctrl_path| std::fs::read_to_string(&ctrl_path).ok())
            .and_then(|ctrl_src| {
                let response_class = find_response_class(&ctrl_src, &method_name)?;
                // Simple name for the component key (last segment of any FQN).
                let simple = response_class
                    .split('\\')
                    .next_back()
                    .unwrap_or(&response_class)
                    .to_string();
                let response_path = resolve_response_file(repo_path, &response_class, &ctrl_src)?;
                let response_src = std::fs::read_to_string(&response_path).ok()?;
                let schema = extract_payload_schema(&response_src);
                // Only use the schema if it has at least one property.
                let has_props = schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|p| !p.is_empty())
                    .unwrap_or(false);
                has_props.then_some((simple, schema))
            });

        if let Some((cname, schema)) = resolved {
            components_schemas.insert(cname.clone(), schema);
            // Wrap the data class inside an ApiResponse envelope schema.
            let wrapped_cname = format!("ApiResponse{cname}");
            components_schemas.insert(
                wrapped_cname.clone(),
                json!({
                    "type": "object",
                    "properties": {
                        "successful": { "type": "boolean" },
                        "data": { "$ref": format!("#/components/schemas/{cname}") },
                        "status": { "type": "integer" },
                        "errorMessage": { "type": ["string", "null"] }
                    }
                }),
            );
            if let Some(path_item) = paths.get_mut(&oas_path) {
                let method = route.method.as_str();
                path_item[method]["responses"]["200"] = json!({
                    "description": "Success",
                    "content": {
                        "application/json": {
                            "schema": { "$ref": format!("#/components/schemas/{wrapped_cname}") }
                        }
                    }
                });
            }
        }
    }

    // ── Pass 4: setJSON literal array schema + engine proxy ──────────────────
    for route in &routes {
        let oas_path = ci4_path_to_oas(&route.path);
        let is_generic = paths
            .get(&oas_path)
            .and_then(|p| p[route.method.as_str()].get("responses"))
            .and_then(|r| r["200"]["content"]["application/json"]["schema"]["$ref"].as_str())
            .map(|r| r == "#/components/schemas/ApiResponse")
            .unwrap_or(false);
        if !is_generic {
            continue;
        }
        if route.controller.is_empty() {
            continue;
        }

        let method_name = route
            .controller
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();
        let Some(ctrl_path) = resolve_controller_file(repo_path, &route.controller) else {
            continue;
        };
        let Ok(ctrl_src) = std::fs::read_to_string(&ctrl_path) else {
            continue;
        };
        let Some(body) = extract_method_body(&ctrl_src, &method_name) else {
            continue;
        };

        if body.contains("setEngineResponseData(") {
            if let Some(path_item) = paths.get_mut(&oas_path) {
                path_item[route.method.as_str()]["responses"]["200"] = json!({
                    "description": "Success",
                    "content": { "application/json": { "schema": engine_proxy_response() } }
                });
            }
            continue;
        }

        // Try AST extraction first; fall back to context-aware string scan.
        let schema = if let Some(php) = PhpFile::parse(&ctrl_src) {
            php.set_json_schema(&method_name).map(|props| {
                json!({
                    "type": "object",
                    "properties": Value::Object(props)
                })
            })
        } else {
            infer_set_json_schema_ctx(body, &ctrl_src, repo_path)
                .or_else(|| infer_set_json_schema(body))
        };
        if let Some(schema) = schema
            && let Some(path_item) = paths.get_mut(&oas_path)
        {
            path_item[route.method.as_str()]["responses"]["200"] = json!({
                "description": "Success",
                "content": { "application/json": { "schema": schema } }
            });
        }
    }

    // ── Pass 5: sendResponse(data: $var->toArray()) → entity $casts ──────────
    for route in &routes {
        let oas_path = ci4_path_to_oas(&route.path);
        let is_generic = paths
            .get(&oas_path)
            .and_then(|p| p[route.method.as_str()].get("responses"))
            .and_then(|r| r["200"]["content"]["application/json"]["schema"]["$ref"].as_str())
            .map(|r| r == "#/components/schemas/ApiResponse")
            .unwrap_or(false);
        if !is_generic {
            continue;
        }
        if route.controller.is_empty() {
            continue;
        }

        let method_name = route
            .controller
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();
        let Some(ctrl_path) = resolve_controller_file(repo_path, &route.controller) else {
            continue;
        };
        let Ok(ctrl_src) = std::fs::read_to_string(&ctrl_path) else {
            continue;
        };
        let Some(body) = extract_method_body(&ctrl_src, &method_name) else {
            continue;
        };

        // Try AST var extraction first; fall back to string scan.
        let var_name = if let Some(php) = PhpFile::parse(&ctrl_src) {
            php.send_response_to_array_var(&method_name)
        } else {
            find_send_response_var_to_array(body)
        };
        let Some(var_name) = var_name else { continue };

        // Entity file tracing is a multi-file hop — still string-based.
        let Some((entity_class, entity_file)) =
            trace_var_to_entity_file(body, &ctrl_src, &var_name, repo_path)
        else {
            continue;
        };
        let Ok(entity_src) = std::fs::read_to_string(&entity_file) else {
            continue;
        };

        // Try AST $casts extraction first; fall back to string scan.
        let entity_schema = if let Some(php) = PhpFile::parse(&entity_src) {
            php.casts_array().map(|casts| {
                let mut props = serde_json::Map::new();
                for (k, v) in &casts {
                    props.insert(
                        crate::scanner::php_parse::snake_to_camel(k),
                        crate::scanner::php_parse::ci4_cast_to_json_schema(v),
                    );
                }
                json!({ "type": "object", "properties": props })
            })
        } else {
            extract_casts_schema(&entity_src)
        };
        let Some(entity_schema) = entity_schema else {
            continue;
        };

        let has_props = entity_schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false);
        if !has_props {
            continue;
        }

        let wrapped_name = format!("ApiResponse{entity_class}");
        components_schemas
            .entry(entity_class.clone())
            .or_insert(entity_schema);
        components_schemas
            .entry(wrapped_name.clone())
            .or_insert_with(|| {
                json!({
                    "type": "object",
                    "properties": {
                        "successful": { "type": "boolean" },
                        "data": { "$ref": format!("#/components/schemas/{entity_class}") },
                        "status": { "type": "integer" },
                        "errorMessage": { "type": ["string", "null"] }
                    }
                })
            });

        if let Some(path_item) = paths.get_mut(&oas_path) {
            path_item[route.method.as_str()]["responses"]["200"] = json!({
                "description": "Success",
                "content": { "application/json": {
                    "schema": { "$ref": format!("#/components/schemas/{wrapped_name}") }
                }}
            });
        }
    }

    // ── Pass 6: multi-hop service method return type tracing ─────────────────
    // Handles `sendResponse(data: $var)` and `sendResponse(data: $obj->method())`
    // where the data comes from a service/model method with a @return annotation.
    for route in &routes {
        let oas_path = ci4_path_to_oas(&route.path);
        let is_generic = paths
            .get(&oas_path)
            .and_then(|p| p[route.method.as_str()].get("responses"))
            .and_then(|r| r["200"]["content"]["application/json"]["schema"]["$ref"].as_str())
            .map(|r| r == "#/components/schemas/ApiResponse")
            .unwrap_or(false);
        if !is_generic {
            continue;
        }
        if route.controller.is_empty() {
            continue;
        }

        let method_name = route
            .controller
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();
        let Some(ctrl_path) = resolve_controller_file(repo_path, &route.controller) else {
            continue;
        };
        let Ok(ctrl_src) = std::fs::read_to_string(&ctrl_path) else {
            continue;
        };
        let Some(body) = extract_method_body(&ctrl_src, &method_name) else {
            continue;
        };

        let Some((data_schema, schema_name)) =
            resolve_send_response_schema(body, &ctrl_src, repo_path)
        else {
            continue;
        };

        // Only use schemas that have meaningful content
        let has_props = data_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|p| !p.is_empty())
            .unwrap_or_else(|| {
                data_schema
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| {
                        t == "array"
                            || t == "string"
                            || t == "integer"
                            || t == "boolean"
                            || t == "number"
                    })
                    .unwrap_or(false)
            });
        if !has_props {
            continue;
        }

        let wrapped_name = format!("ApiResponse{schema_name}");
        components_schemas
            .entry(schema_name.clone())
            .or_insert(data_schema);
        components_schemas
            .entry(wrapped_name.clone())
            .or_insert_with(|| {
                json!({
                    "type": "object",
                    "properties": {
                        "successful": { "type": "boolean" },
                        "data": { "$ref": format!("#/components/schemas/{schema_name}") },
                        "status": { "type": "integer" },
                        "errorMessage": { "type": ["string", "null"] }
                    }
                })
            });
        if let Some(path_item) = paths.get_mut(&oas_path) {
            path_item[route.method.as_str()]["responses"]["200"] = json!({
                "description": "Success",
                "content": { "application/json": {
                    "schema": { "$ref": format!("#/components/schemas/{wrapped_name}") }
                }}
            });
        }
    }

    // Inject standard envelope schemas
    components_schemas.insert(
        "ApiResponse".to_string(),
        json!({
            "type": "object",
            "properties": {
                "successful": { "type": "boolean" },
                "data": { "description": "Response payload — type varies by endpoint" },
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
    controller: String,
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
            // Prefer AST-based extraction; fall back to string scanning when
            // the php-ast feature is disabled or tree-sitter can't parse the file.
            if let Some(php) = PhpFile::parse(&src) {
                for r in php.route_registrations() {
                    out.push(Ci4Route {
                        method: r.method,
                        path: r.path,
                        filters: r.filters,
                        controller: r.controller,
                    });
                }
            } else {
                parse_routes(&src, out);
            }
        }
    }
    Ok(())
}

/// Extract all `$routes->METHOD(...)` calls from a PHP file.
fn parse_routes(src: &str, out: &mut Vec<Ci4Route>) {
    let methods = ["get", "post", "put", "delete", "patch", "options", "head"];
    let mut pos = 0;
    let _bytes = src.as_bytes();

    while pos < src.len() {
        // Find `$routes->`
        if let Some(rel) = src[pos..].find("$routes->") {
            pos += rel + "$routes->".len();

            // Check method name
            let rest = &src[pos..];
            let mut found_method = None;
            for m in &methods {
                if let Some(after) = rest.strip_prefix(m) {
                    let after = after.trim_start();
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
            if let Some(route) = parse_route_call(method, call) {
                out.push(route);
            }
        } else {
            break;
        }
    }
}

/// Extract balanced delimiters content (not including the delimiters).
/// Extract the content between the first matched pair of `open`/`close`
/// delimiters in `src`, where `src` must start with `open`.
/// Returns the inner content (not including the delimiters themselves).
/// Used to isolate PHP argument lists `(...)` and array literals `[...]`.
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

/// Parse a single `$routes->METHOD(...)` call body into a `Ci4Route`.
/// The first quoted string is the path, the second is the controller ref
/// (stripped of capture-group suffixes like `/$1`), and the third argument
/// (if present) is the options array from which we extract auth filters.
fn parse_route_call(method: &str, call: &str) -> Option<Ci4Route> {
    // call is the content inside the outer parens, e.g.:
    //   'admin/api/v1/shop', 'Api\V1\Shop::getShopObject', ['filter' => ['auth']]
    let path = extract_first_quoted(call)?;

    // Extract second quoted string: the controller ref
    let controller = extract_second_quoted(call)
        .map(|s| {
            // Strip /$1, /$2, etc. suffixes
            if let Some(slash_pos) = s.find("/$") {
                s[..slash_pos].to_string()
            } else {
                s
            }
        })
        .unwrap_or_default();

    // Extract filter array: look for 'filter' => [...]
    let filters = extract_filters(call);

    Some(Ci4Route {
        method: method.to_string(),
        path,
        filters,
        controller,
    })
}

fn extract_first_quoted(src: &str) -> Option<String> {
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
                    // Preserve backslash — PHP single-quoted strings only escape \\ and \'
                    escaped = true;
                    result.push('\\');
                } else if c == quote {
                    return Some(result);
                } else {
                    result.push(c);
                }
            }
            let _ = i;
            break;
        }
    }
    None
}

fn extract_second_quoted(src: &str) -> Option<String> {
    let mut quote_count = 0;
    let mut chars = src.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '\'' || ch == '"' {
            quote_count += 1;
            let quote = ch;
            let mut escaped = false;
            let mut result = String::new();
            let is_second = quote_count == 2;
            for (_, c) in chars.by_ref() {
                if escaped {
                    if is_second {
                        result.push(c);
                    }
                    escaped = false;
                } else if c == '\\' {
                    // Keep backslash — PHP namespace separators are literal backslashes
                    // in single-quoted strings. Losing them breaks controller file lookup.
                    escaped = true;
                    if is_second {
                        result.push('\\');
                    }
                } else if c == quote {
                    if is_second {
                        return Some(result);
                    }
                    break;
                } else if is_second {
                    result.push(c);
                }
            }
        }
    }
    None
}

/// Extract the string values from the `'filter'` key of a CI4 route options
/// array.  The filter list drives both auth security requirements
/// (`auth`, `shop-admin`, …) and JSON-Schema body validation
/// (`json-schema:/V1/Foo/Bar.json`).  Both are single-string and array forms:
/// `'filter' => 'auth'` and `'filter' => ['auth', 'csrf']`.
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
                    // Preserve backslash — PHP single-quoted strings only escape \\ and \'
                    escaped = true;
                    val.push('\\');
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

    replace_path_params(&oas)
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

/// Map CI4 auth filter names to OAS security requirement objects.
/// admin-api uses several filter names that all imply bearer token auth;
/// routes without any recognised auth filter get no security requirement.
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

/// Derive an OAS tag from a CI4 path.  We strip the versioned API prefix
/// (`admin/api/v1/`) and use the first remaining segment as the domain tag
/// so that Swagger UI and API consumers can group by resource type.
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
    Value::Array(tags.into_iter().map(|t| json!({ "name": t })).collect())
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

// ── Controller / Payload / Response resolution ───────────────────────────────

/// Resolve a controller ref like `Api\V1\RebuyAssistant` to its PHP file path.
fn resolve_controller_file(repo_path: &Path, ctrl_ref: &str) -> Option<PathBuf> {
    // Strip everything after and including `::`
    let class_part = if let Some(pos) = ctrl_ref.find("::") {
        &ctrl_ref[..pos]
    } else {
        ctrl_ref
    };
    // `Api\V1\RebuyAssistant` → `apps/ci4/app/Controllers/Api/V1/RebuyAssistant.php`
    let rel = class_part.replace('\\', "/");
    let path = repo_path
        .join("apps/ci4/app/Controllers")
        .join(&rel)
        .with_extension("php");
    if path.exists() { Some(path) } else { None }
}

/// Extract the body of a named method from PHP source.
fn extract_method_body<'a>(src: &'a str, method_name: &str) -> Option<&'a str> {
    let needle = format!("function {}(", method_name);
    let func_pos = src.find(&needle)?;
    // Advance past the function signature to the opening `{`
    let after_sig = &src[func_pos + needle.len()..];
    let brace_pos = after_sig.find('{')?;
    let body_start = func_pos + needle.len() + brace_pos;
    extract_balanced(&src[body_start..], '{', '}').map(|s| {
        // Return a slice of the original src rather than a derived slice
        let offset = body_start + 1; // skip the opening brace
        let end = offset + s.len();
        &src[offset..end]
    })
}

/// Find the Payload class name used in a method body.
fn find_payload_class(ctrl_src: &str, method_name: &str) -> Option<String> {
    let body = extract_method_body(ctrl_src, method_name)?;

    // Pattern 1: SomePayload::buildFromRequestPayload
    if let Some(pos) = body.find("::buildFromRequestPayload") {
        let before = &body[..pos];
        let class = before.split_whitespace().last()?;
        // Clean off any `$var = ` prefix if it leaked in
        let class = class.split('=').next_back().unwrap_or(class).trim();
        if !class.is_empty() && class.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Some(class.to_string());
        }
    }

    // Pattern 2: new SomePayload(
    if let Some(pos) = body.find("new ") {
        let after = &body[pos + 4..];
        let end = after.find('(')?;
        let class = after[..end].trim();
        if class.ends_with("Payload") && class.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Some(class.to_string());
        }
    }

    // Pattern 3: any SomePayload:: usage
    let mut search = body;
    while let Some(pos) = search.find("Payload::") {
        let before = &search[..pos];
        let class = before.split_whitespace().last().unwrap_or("").trim();
        let class = class.split('(').next_back().unwrap_or(class).trim();
        if !class.is_empty() && class.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Some(class.to_string());
        }
        search = &search[pos + 1..];
    }

    None
}

/// Find the response class declared via `#[ApiResponse(ClassName::class)]` above a method.
fn find_response_class(ctrl_src: &str, method_name: &str) -> Option<String> {
    let needle = format!("function {}(", method_name);
    let method_pos = ctrl_src.find(&needle)?;

    let before = &ctrl_src[..method_pos];
    let attr_prefix = "#[ApiResponse(";
    let attr_pos = before.rfind(attr_prefix)?;

    // Reject if another function declaration sits between the attribute and this method.
    let between = &ctrl_src[attr_pos + attr_prefix.len()..method_pos];
    if between.contains("function ") {
        return None;
    }

    // Extract the class name from #[ApiResponse(ClassName::class)]
    let after_attr = &ctrl_src[attr_pos + attr_prefix.len()..];
    let end = after_attr.find("::class")?;
    let raw = after_attr[..end].trim();

    // Strip a leading backslash from fully-qualified names (\App\...).
    let class_name = raw.trim_start_matches('\\').to_string();

    if class_name.is_empty() {
        return None;
    }
    Some(class_name)
}

/// Resolve a Response class to its file path.
/// Accepts either a simple name (resolved via `use` statements) or a FQN.
fn resolve_response_file(
    repo_path: &Path,
    response_class: &str,
    ctrl_src: &str,
) -> Option<PathBuf> {
    // FQN path: App\Responses\... → resolve directly without needing a use stmt.
    if response_class.contains('\\') {
        if let Some(p) = namespace_to_path(repo_path, response_class)
            && p.exists()
        {
            return Some(p);
        }
        return None;
    }

    // Simple name: scan `use` statements for one whose last component matches.
    for line in ctrl_src.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("use ") {
            continue;
        }
        let ns = trimmed
            .trim_start_matches("use ")
            .trim_end_matches(';')
            .trim();
        let last = ns.split('\\').next_back().unwrap_or("");
        if last != response_class {
            continue;
        }
        if let Some(p) = namespace_to_path(repo_path, ns)
            && p.exists()
        {
            return Some(p);
        }
    }
    None
}

/// Resolve a Payload class name to its file path by scanning `use` statements.
fn resolve_payload_file(repo_path: &Path, payload_class: &str, ctrl_src: &str) -> Option<PathBuf> {
    // Scan `use` statements for one ending in the payload class name
    for line in ctrl_src.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("use ") {
            continue;
        }
        // Strip "use " prefix and trailing ";"
        let ns = trimmed
            .trim_start_matches("use ")
            .trim_end_matches(';')
            .trim();
        // The last component must match the payload class
        let last = ns.split('\\').next_back().unwrap_or("");
        if last != payload_class {
            continue;
        }

        let path = namespace_to_path(repo_path, ns);
        if let Some(p) = path
            && p.exists()
        {
            return Some(p);
        }
    }
    None
}

/// Map a fully-qualified PHP namespace to a filesystem path.
fn namespace_to_path(repo_path: &Path, ns: &str) -> Option<PathBuf> {
    if let Some(rest) = ns.strip_prefix("App\\") {
        // App\Payloads\... → apps/ci4/app/Payloads/...
        let rel = rest.replace('\\', "/");
        return Some(
            repo_path
                .join("apps/ci4/app")
                .join(&rel)
                .with_extension("php"),
        );
    }
    if let Some(rest) = ns.strip_prefix("RebuyCore\\") {
        // RebuyCore\... → apps/ci4/vendor/rebuy/core-ci4/src/...
        let rel = rest.replace('\\', "/");
        return Some(
            repo_path
                .join("apps/ci4/vendor/rebuy/core-ci4/src")
                .join(&rel)
                .with_extension("php"),
        );
    }
    None
}

// ── Pass 4/5 helpers ──────────────────────────────────────────────────────────

/// Inline engine-proxy response schema: data is a runtime-assembled array
/// from the engine layer — we can't know the element shape without deep tracing.
fn engine_proxy_response() -> Value {
    json!({
        "type": "object",
        "properties": {
            "successful": { "type": "boolean" },
            "data": { "type": "array", "items": {} },
            "status": { "type": "integer" },
            "errorMessage": { "type": ["string", "null"] }
        }
    })
}

/// Find the best-typed schema from `->setJSON([literal array])` return
/// statements in a method body.  Picks the schema with the most properties
/// (the "richest" success path).
///
/// Handles both single-line `->setJSON([` and multiline `->setJSON(\n    [`
/// formats — the latter is common in the admin-api reports controllers.
fn infer_set_json_schema(body: &str) -> Option<Value> {
    let mut best: serde_json::Map<String, Value> = serde_json::Map::new();
    for call_pat in &["->setJSON(", "->setJson("] {
        let mut pos = 0;
        while let Some(rel) = body[pos..].find(call_pat) {
            let after_paren = pos + rel + call_pat.len();
            // Skip whitespace (including newlines) between `(` and `[`
            let trimmed = body[after_paren..].trim_start();
            if trimmed.starts_with('[') {
                let array_start = after_paren + (body[after_paren..].len() - trimmed.len());
                if let Some(inner) = extract_balanced(&body[array_start..], '[', ']') {
                    let props = parse_php_array_schema(inner);
                    if props.len() > best.len() {
                        best = props;
                    }
                }
            // Pattern B: ->setJSON((new ApiResponse($ok, $data, $status, $msg))->toJson())
            // Wrap data in the standard ApiResponse envelope using the variable name for $data.
            } else if (trimmed.starts_with("(new ApiResponse(")
                || trimmed.starts_with("(new \\ApiResponse("))
                && let Some(args_inner) = extract_balanced(trimmed, '(', ')')
            {
                // ApiResponse(bool $successful, mixed $data, int $status, ?string $errorMessage)
                let args: Vec<&str> = args_inner.splitn(4, ',').collect();
                if args.len() >= 2 {
                    let data_arg = args[1].trim();
                    let data_schema = infer_php_literal_schema(data_arg);
                    // Build envelope properties with typed data field
                    let mut props = serde_json::Map::new();
                    props.insert("successful".to_string(), json!({ "type": "boolean" }));
                    props.insert("data".to_string(), data_schema);
                    props.insert("status".to_string(), json!({ "type": "integer" }));
                    props.insert(
                        "errorMessage".to_string(),
                        json!({ "type": ["string", "null"] }),
                    );
                    if props.len() > best.len() {
                        best = props;
                    }
                }
            }
            pos += rel + 1;
        }
    }
    if best.is_empty() {
        return None;
    }
    Some(json!({ "type": "object", "properties": best }))
}

/// Parse a PHP associative array body (content between `[` and `]`) into a
/// JSON Schema `properties` map.  Only processes literal scalar values to
/// infer types; variables are emitted as `{}` (unknown).
fn parse_php_array_schema(inner: &str) -> serde_json::Map<String, Value> {
    let mut props = serde_json::Map::new();
    let mut remaining = inner;
    while let Some(qp) = remaining.find(['\'', '"']) {
        let quote = remaining.as_bytes()[qp] as char;
        let after = &remaining[qp + 1..];
        let Some(key_end) = find_closing_quote(after, quote) else {
            break;
        };
        let key = &after[..key_end];
        if key.is_empty() {
            remaining = &after[key_end + 1..];
            continue;
        }

        let after_key = after[key_end + 1..].trim_start();
        if !after_key.starts_with("=>") {
            remaining = after_key;
            continue;
        }
        let val_src = after_key[2..].trim_start();

        // Value-based inference first; fall back to key-name hint when value is opaque.
        let schema = {
            let by_val = infer_php_literal_schema(val_src);
            if by_val.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                // Value gave no info — try key name
                let hint = infer_schema_from_name(key);
                if !hint.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                    hint
                } else {
                    by_val
                }
            } else {
                by_val
            }
        };
        props.insert(key.to_string(), schema);
        remaining = skip_php_value_to_comma(val_src);
    }
    props
}

/// Find the closing quote character in `src`, skipping backslash escapes.
fn find_closing_quote(src: &str, quote: char) -> Option<usize> {
    let mut i = 0;
    let bytes = src.as_bytes();
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\\' {
            i += 2;
            continue;
        }
        if c == quote {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Infer a JSON Schema type from a PHP name (variable or key name).
/// Uses naming conventions when the actual value isn't statically knowable.
fn infer_schema_from_name(name: &str) -> Value {
    let lower = name.to_lowercase();
    // Boolean indicators
    if matches!(
        lower.as_str(),
        "successful"
            | "success"
            | "ok"
            | "enabled"
            | "active"
            | "deleted"
            | "found"
            | "exists"
            | "is_valid"
            | "valid"
            | "published"
            | "visible"
    ) || lower.starts_with("is_")
        || lower.starts_with("has_")
        || lower.starts_with("can_")
    {
        return json!({ "type": "boolean" });
    }
    // Integer indicators
    if matches!(
        lower.as_str(),
        "count"
            | "total"
            | "id"
            | "status"
            | "code"
            | "page"
            | "limit"
            | "offset"
            | "per_page"
            | "current_page"
            | "last_page"
            | "total_pages"
            | "size"
            | "length"
    ) || lower.ends_with("_id")
        || lower.ends_with("_count")
        || lower.ends_with("_total")
        || lower.ends_with("_status")
        || lower.ends_with("_code")
    {
        return json!({ "type": "integer" });
    }
    // String indicators
    if matches!(
        lower.as_str(),
        "error"
            | "errormessage"
            | "error_message"
            | "message"
            | "msg"
            | "name"
            | "title"
            | "description"
            | "url"
            | "handle"
            | "type"
            | "key"
            | "token"
            | "value"
            | "label"
            | "slug"
            | "email"
            | "phone"
            | "address"
            | "currency"
            | "locale"
            | "timezone"
            | "format"
            | "mode"
            | "state"
            | "reason"
            | "note"
            | "comment"
            | "text"
    ) || lower.ends_with("_name")
        || lower.ends_with("_title")
        || lower.ends_with("_url")
        || lower.ends_with("_key")
        || lower.ends_with("_token")
        || lower.ends_with("_type")
        || lower.ends_with("_id") && lower.len() > 3 && lower.ends_with("_id")
    {
        // Don't override integer for _id if already caught above
        if lower.ends_with("_name")
            || lower.ends_with("_title")
            || lower.ends_with("_url")
            || lower.ends_with("_key")
            || lower.ends_with("_token")
            || lower.ends_with("_type")
        {
            return json!({ "type": "string" });
        }
        return json!({ "type": "string" });
    }
    // Array indicators — plural nouns or common collection names
    if matches!(
        lower.as_str(),
        "data"
            | "items"
            | "results"
            | "list"
            | "records"
            | "rows"
            | "entries"
            | "collection"
            | "set"
            | "batch"
            | "ids"
            | "tags"
            | "errors"
            | "warnings"
            | "attributes"
            | "options"
            | "filters"
            | "params"
            | "headers"
            | "fields"
    ) || (lower.ends_with('s')
        && lower.len() > 3
        && !matches!(
            lower.as_str(),
            "status" | "class" | "process" | "address" | "access"
        ))
    {
        return json!({ "type": "array", "items": {} });
    }
    json!({})
}

/// Map a PHP literal value to a JSON Schema type fragment.
/// Handles scalar literals, array literals, expressions, and variable name hints.
fn infer_php_literal_schema(val: &str) -> Value {
    if val.starts_with("true") || val.starts_with("false") {
        json!({ "type": "boolean" })
    } else if val.starts_with(['\'', '"']) {
        json!({ "type": "string" })
    } else if val.starts_with(|c: char| c.is_ascii_digit()) {
        if val.contains('.') {
            json!({ "type": "number" })
        } else {
            json!({ "type": "integer" })
        }
    } else if val.starts_with("null") {
        json!({ "type": "null" })
    } else if val.starts_with('[') || val.starts_with("array(") {
        let (open, close) = if val.starts_with('[') {
            ('[', ']')
        } else {
            ('(', ')')
        };
        if let Some(inner) = extract_balanced(val, open, close) {
            // Associative array → object
            let props = parse_php_array_schema(inner);
            if !props.is_empty() {
                return json!({ "type": "object", "properties": Value::Object(props) });
            }
            // Sequential array — try to infer item type from first element
            let first = inner.trim();
            if !first.is_empty() {
                let item_schema = infer_php_literal_schema(first);
                return json!({ "type": "array", "items": item_schema });
            }
        }
        json!({ "type": "array", "items": {} })
    // Expression-based inference: cast operators and well-known functions
    } else if val.starts_with("(int)")
        || val.starts_with("(integer)")
        || val.starts_with("intval(")
        || val.starts_with("count(")
        || val.starts_with("sizeof(")
        || val.starts_with("strlen(")
    {
        json!({ "type": "integer" })
    } else if val.starts_with("(float)")
        || val.starts_with("(double)")
        || val.starts_with("floatval(")
    {
        json!({ "type": "number" })
    } else if val.starts_with("(bool)")
        || val.starts_with("(boolean)")
        || val.starts_with("boolval(")
    {
        json!({ "type": "boolean" })
    } else if val.starts_with("(string)")
        || val.starts_with("strval(")
        || val.starts_with("sprintf(")
        || val.starts_with("implode(")
        || val.starts_with("json_encode(")
    {
        json!({ "type": "string" })
    } else if val.starts_with("array_")
        || val.starts_with("array_merge(")
        || val.starts_with("array_map(")
        || val.starts_with("array_filter(")
        || val.starts_with("array_values(")
    {
        json!({ "type": "array", "items": {} })
    // Variable: use name-based heuristic
    } else if let Some(rest) = val.strip_prefix('$') {
        let name = rest
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or("");
        infer_schema_from_name(name)
    } else {
        json!({})
    }
}

/// Advance past the current PHP value to the character after the next
/// depth-0 comma, or to "" if the array ends.
fn skip_php_value_to_comma(src: &str) -> &str {
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut str_char = '\0';
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == str_char {
                in_str = false;
            }
        } else {
            match c {
                '\'' | '"' => {
                    in_str = true;
                    str_char = c;
                }
                '[' | '(' | '{' => depth += 1,
                ']' | ')' | '}' => {
                    depth -= 1;
                    if depth < 0 {
                        return &src[i..];
                    }
                }
                ',' if depth == 0 => return &src[i + 1..],
                _ => {}
            }
        }
        i += 1;
    }
    ""
}

/// Locate `sendResponse(data: $varName->toArray(` and return `varName`.
/// Named-argument form is standard in CI4 admin-api controllers.
fn find_send_response_var_to_array(body: &str) -> Option<String> {
    let mut search = body;
    while let Some(rel) = search.find("sendResponse(") {
        let after_sr = &search[rel + "sendResponse(".len()..];
        if let Some(data_rel) = after_sr.find("data:") {
            // Guard: ensure no unmatched `)` between sendResponse( and data:
            let depth: i32 = after_sr[..data_rel]
                .chars()
                .map(|c| match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                })
                .sum();
            if depth >= 0 {
                let after_data = after_sr[data_rel + "data:".len()..].trim_start();
                if let Some(v) = extract_var_to_array(after_data) {
                    return Some(v);
                }
            }
        }
        search = &search[rel + 1..];
    }
    None
}

/// If `src` starts with `$varName->toArray(`, return `varName`.
fn extract_var_to_array(src: &str) -> Option<String> {
    if !src.starts_with('$') {
        return None;
    }
    let rest = &src[1..];
    let var_end = rest
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    let var_name = &rest[..var_end];
    if var_name.is_empty() {
        return None;
    }
    let after_var = rest[var_end..].trim_start();
    if after_var.starts_with("->") && after_var[2..].trim_start().starts_with("toArray(") {
        return Some(var_name.to_string());
    }
    None
}

/// 3-hop trace: `$var` → `$model->method()` → `new ModelClass()` →
/// `$returnType = Entity::class` → entity file.
/// Falls back to `$var = new Entity()` (1-hop).
fn trace_var_to_entity_file(
    body: &str,
    ctrl_src: &str,
    var_name: &str,
    repo_path: &Path,
) -> Option<(String, PathBuf)> {
    let assign_pat = format!("${var_name} = $");
    if let Some(pos) = body.find(&assign_pat) {
        let after = &body[pos + assign_pat.len()..];
        let model_end = after
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after.len());
        let model_var = &after[..model_end];
        if !model_var.is_empty()
            && let Some(r) = resolve_via_model(body, ctrl_src, model_var, repo_path)
        {
            return Some(r);
        }
    }
    let new_pat = format!("${var_name} = new ");
    if let Some(pos) = body.find(&new_pat) {
        let after = &body[pos + new_pat.len()..];
        let class_end = after
            .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '\\')
            .unwrap_or(after.len());
        let class_name = &after[..class_end];
        if !class_name.is_empty()
            && let Some(f) = resolve_class_file(repo_path, class_name, ctrl_src)
        {
            let ec = class_name
                .split('\\')
                .next_back()
                .unwrap_or(class_name)
                .to_string();
            return Some((ec, f));
        }
    }
    None
}

/// `$modelVar = new ModelClass()` → read `$returnType` → resolve entity.
fn resolve_via_model(
    body: &str,
    ctrl_src: &str,
    model_var: &str,
    repo_path: &Path,
) -> Option<(String, PathBuf)> {
    let new_pat = format!("${model_var} = new ");
    let pos = body.find(&new_pat)?;
    let after = &body[pos + new_pat.len()..];
    let class_end = after
        .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '\\')
        .unwrap_or(after.len());
    let model_class = &after[..class_end];
    if model_class.is_empty() {
        return None;
    }

    let model_file = resolve_class_file(repo_path, model_class, ctrl_src)?;
    let model_src = std::fs::read_to_string(&model_file).ok()?;

    let full_entity = extract_return_type(&model_src)?;
    let entity_class = full_entity
        .split('\\')
        .next_back()
        .unwrap_or(&full_entity)
        .to_string();
    let entity_file = resolve_class_file(repo_path, &full_entity, &model_src)?;
    Some((entity_class, entity_file))
}

/// Extract `protected $returnType = ClassName::class` (or quoted form) from a
/// CI4 model source.
fn extract_return_type(src: &str) -> Option<String> {
    let needle = "protected $returnType = ";
    let pos = src.find(needle)?;
    let after = src[pos + needle.len()..].trim_start();
    if let Some(cc) = after.find("::class") {
        let class = after[..cc].trim().trim_start_matches('\\');
        if !class.is_empty() {
            return Some(class.to_string());
        }
    }
    if after.starts_with(['\'', '"']) {
        let q = after.as_bytes()[0] as char;
        if let Some(end) = find_closing_quote(&after[1..], q) {
            return Some(after[1..end + 1].to_string());
        }
    }
    None
}

/// Resolve a simple class name (or FQN) to a filesystem path, honouring
/// `use ClassName as Alias` imports in `src_with_uses`.
fn resolve_class_file(repo_path: &Path, class_name: &str, src_with_uses: &str) -> Option<PathBuf> {
    if class_name.contains('\\') {
        let p = namespace_to_path(repo_path, class_name)?;
        return p.exists().then_some(p);
    }
    for line in src_with_uses.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("use ") {
            continue;
        }
        let ns_raw = trimmed
            .trim_start_matches("use ")
            .trim_end_matches(';')
            .trim();
        let (actual_ns, resolved_name) = if let Some(as_pos) = ns_raw.find(" as ") {
            (
                ns_raw[..as_pos].trim(),
                ns_raw[as_pos + " as ".len()..].trim(),
            )
        } else {
            let last = ns_raw.split('\\').next_back().unwrap_or("");
            (ns_raw, last)
        };
        if resolved_name != class_name {
            continue;
        }
        if let Some(p) = namespace_to_path(repo_path, actual_ns)
            && p.exists()
        {
            return Some(p);
        }
    }
    None
}

/// Parse `protected $casts = [...]` from a CI4 Entity source into a JSON
/// Schema properties map.  Keys are snake_case → camelCase converted.
/// CI4 cast types: `?int`, `?string`, `?datetime`, `?json-array`, etc.
fn extract_casts_schema(entity_src: &str) -> Option<Value> {
    let needle = "protected $casts = [";
    let pos = entity_src.find(needle)?;
    let array_start = pos + needle.len() - 1;
    let inner = extract_balanced(&entity_src[array_start..], '[', ']')?;

    let mut properties = serde_json::Map::new();
    let mut remaining = inner;
    while let Some(qp) = remaining.find(['\'', '"']) {
        let quote = remaining.as_bytes()[qp] as char;
        let after = &remaining[qp + 1..];
        let Some(key_end) = find_closing_quote(after, quote) else {
            break;
        };
        let key = &after[..key_end];
        if key.is_empty() {
            remaining = &after[key_end + 1..];
            continue;
        }

        let after_key = after[key_end + 1..].trim_start();
        if !after_key.starts_with("=>") {
            remaining = after_key;
            continue;
        }
        let val_src = after_key[2..].trim_start();

        if val_src.starts_with(['\'', '"']) {
            let vq = val_src.as_bytes()[0] as char;
            if let Some(val_end) = find_closing_quote(&val_src[1..], vq) {
                let cast_type = &val_src[1..val_end + 1];
                properties.insert(
                    crate::scanner::php_parse::snake_to_camel(key),
                    crate::scanner::php_parse::ci4_cast_to_json_schema(cast_type),
                );
                remaining = skip_php_value_to_comma(&val_src[val_end + 2..]);
                continue;
            }
        }
        remaining = skip_php_value_to_comma(val_src);
    }

    if properties.is_empty() {
        return None;
    }
    Some(json!({ "type": "object", "properties": properties }))
}

/// Extract a JSON Schema from a PHP Payload/Response class source.
/// Handles:
///   - `protected TYPE $field` / `protected ?TYPE $field`
///   - `@var Type[]` docblock above the field → array with typed items
///   - `@var Type` docblock → overrides the declared type
///   - Object types → emitted as `{ "type": "object", "description": "ClassName" }`
fn extract_payload_schema(payload_src: &str) -> Value {
    let skip_fields = ["id", "owner"];
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    let lines: Vec<&str> = payload_src.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("protected ") {
            continue;
        }
        let rest = &trimmed["protected ".len()..];
        let nullable = rest.starts_with('?');
        let rest = if nullable { &rest[1..] } else { rest };

        let mut parts = rest.splitn(2, '$');
        let type_part = parts.next().unwrap_or("").trim();
        let name_part = parts.next().unwrap_or("");
        let field_name = name_part
            .split(|c: char| c.is_whitespace() || c == '=' || c == ';')
            .next()
            .unwrap_or("")
            .trim();

        if field_name.is_empty() || type_part.is_empty() {
            continue;
        }
        if skip_fields.contains(&field_name) {
            continue;
        }

        // Scan backwards for the nearest @var docblock
        let var_annotation = (0..idx).rev().take(8).find_map(|i| {
            let l = lines[i].trim();
            if l.contains("@var ") {
                let after = &l[l.find("@var ").unwrap() + 5..];
                let token = after
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('*');
                if !token.is_empty() {
                    Some(token.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        });

        let prop_schema = if let Some(var_type) = var_annotation {
            // @var Type[] → array with items
            if var_type.ends_with("[]") {
                let inner = var_type.trim_end_matches("[]");
                let item_schema = match php_type_to_json(inner) {
                    "object" => json!({ "type": "object", "description": inner }),
                    t => json!({ "type": t }),
                };
                json!({ "type": "array", "items": item_schema })
            } else {
                match php_type_to_json(&var_type) {
                    "array" => json!({ "type": "array", "items": {} }),
                    "object" => json!({ "type": "object", "description": var_type }),
                    t => json!({ "type": t }),
                }
            }
        } else {
            match php_type_to_json(type_part) {
                "array" => json!({ "type": "array", "items": {} }),
                "object" => json!({ "type": "object", "description": type_part }),
                t => json!({ "type": t }),
            }
        };

        properties.insert(field_name.to_string(), prop_schema);

        if !nullable {
            required.push(Value::String(field_name.to_string()));
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(schema)
}

/// Map PHP scalar type hints to JSON Schema type strings.
/// Used for Payload class property inference; does not handle nullable (`?`)
/// since that's handled by the caller via `required` array presence.
fn php_type_to_json(php_type: &str) -> &'static str {
    match php_type {
        "int" | "integer" => "integer",
        "string" => "string",
        "bool" | "boolean" => "boolean",
        "float" | "double" => "number",
        "array" => "array",
        _ => "object",
    }
}

/// Extract fields from getJSON(true) usage in a method body.
fn extract_getjson_fields(method_body: &str) -> Value {
    let mut properties = serde_json::Map::new();

    // Patterns: $body['field'], $payload['field'], $data['field'],
    // or $this->request->getJSON(true)['field']
    let mut search = method_body;
    while let Some(pos) = search.find("['") {
        let after = &search[pos + 2..];
        if let Some(end) = after.find("']") {
            let field = &after[..end];
            if !field.is_empty()
                && field.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !properties.contains_key(field)
            {
                // Only include if preceded by a typical body variable or getJSON call
                let before = &search[..pos];
                let last_token = before
                    .split(|c: char| c.is_whitespace() || c == '(' || c == ',')
                    .rfind(|s| !s.is_empty())
                    .unwrap_or("");
                if last_token.starts_with('$')
                    || last_token.ends_with("getJSON(true)")
                    || last_token.contains("getJSON")
                {
                    properties.insert(field.to_string(), json!({ "type": "string" }));
                }
            }
            search = &search[pos + 2 + end + 2..];
        } else {
            break;
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    Value::Object(schema)
}

// ── Pass 4 context-aware helpers ─────────────────────────────────────────────

/// Context-aware variant of `infer_set_json_schema`.
/// Passes controller source and repo path into the array schema parser so that
/// property values like `$this->service->method(...)` can be resolved.
fn infer_set_json_schema_ctx(body: &str, ctrl_src: &str, repo_path: &Path) -> Option<Value> {
    let mut best: serde_json::Map<String, Value> = serde_json::Map::new();
    for call_pat in &["->setJSON(", "->setJson("] {
        let mut pos = 0;
        while let Some(rel) = body[pos..].find(call_pat) {
            let after_paren = pos + rel + call_pat.len();
            let rest = body[after_paren..].trim_start();
            let open = if rest.starts_with('[') {
                '['
            } else if rest.starts_with('(') {
                '('
            } else {
                pos += rel + 1;
                continue;
            };
            let close = if open == '[' { ']' } else { ')' };
            let inner = match extract_balanced(rest, open, close) {
                Some(i) => i,
                None => {
                    pos += rel + 1;
                    continue;
                }
            };
            // Only process associative arrays
            if !inner.contains("=>") {
                pos += rel + 1;
                continue;
            }
            let props = parse_array_schema_ctx(inner, body, ctrl_src, repo_path);
            if props.len() > best.len() {
                best = props;
            }
            pos += rel + 1;
        }
    }
    if best.is_empty() {
        return None;
    }
    Some(json!({ "type": "object", "properties": Value::Object(best) }))
}

/// Context-aware `parse_php_array_schema`: uses `infer_expr_ctx` for each value
/// so that service method calls and other complex expressions can be resolved.
fn parse_array_schema_ctx(
    inner: &str,
    body: &str,
    ctrl_src: &str,
    repo_path: &Path,
) -> serde_json::Map<String, Value> {
    let mut props = serde_json::Map::new();
    let mut remaining = inner;
    while let Some(qp) = remaining.find(['\'', '"']) {
        let quote = remaining.as_bytes()[qp] as char;
        let after = &remaining[qp + 1..];
        let Some(key_end) = find_closing_quote(after, quote) else {
            break;
        };
        let key = &after[..key_end];
        if key.is_empty() {
            remaining = &after[key_end + 1..];
            continue;
        }

        let after_key = after[key_end + 1..].trim_start();
        if !after_key.starts_with("=>") {
            remaining = after_key;
            continue;
        }
        let val_src = after_key[2..].trim_start();

        let schema = infer_expr_ctx(val_src, body, ctrl_src, repo_path);
        props.insert(key.to_string(), schema);
        remaining = skip_php_value_to_comma(val_src);
    }
    props
}

/// Context-aware expression inferrer. Extends `infer_php_literal_schema` with:
/// - `$this->prop->method(...)` → resolve property class + method return type
/// - `$var->method(...)` → resolve var class from body + method return type
/// - Falls back to `infer_php_literal_schema` + key-name hint if resolution fails.
fn infer_expr_ctx(val: &str, body: &str, ctrl_src: &str, repo_path: &Path) -> Value {
    let trimmed = val.trim();

    // $this->prop->method(...) — property access chain
    if let Some(rest) = trimmed.strip_prefix("$this->") {
        let prop_end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let prop = &rest[..prop_end];
        let after_prop = rest[prop_end..].trim_start();

        if let Some(method_chain) = after_prop.strip_prefix("->") {
            let method_end = method_chain
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(method_chain.len());
            let method = &method_chain[..method_end];
            if !prop.is_empty()
                && !method.is_empty()
                && let Some(schema) =
                    resolve_this_prop_method_schema(ctrl_src, prop, method, repo_path)
            {
                return schema;
            }
        }
        // $this->prop alone or unresolved — name hint on prop
        if !prop.is_empty() {
            let hint = infer_schema_from_name(prop);
            if !hint.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                return hint;
            }
        }
    }

    // $var->method(...) — variable access chain
    if trimmed.starts_with('$') && !trimmed.starts_with("$this") {
        let rest = &trimmed[1..];
        let var_end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let var_name = &rest[..var_end];
        let after_var = rest[var_end..].trim_start();
        if let Some(method_chain) = after_var.strip_prefix("->") {
            let method_end = method_chain
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(method_chain.len());
            let method = &method_chain[..method_end];
            if !var_name.is_empty()
                && !method.is_empty()
                && let Some(class) = resolve_var_class(body, var_name, ctrl_src, repo_path)
                && let Some(result) = resolve_method_schema(repo_path, &class, method, ctrl_src)
            {
                return result.0;
            }
        }
    }

    // ClassName::staticMethod(...)
    if let Some((class, method)) = parse_static_call(trimmed)
        && let Some(result) = resolve_method_schema(repo_path, &class, &method, ctrl_src)
    {
        return result.0;
    }

    // Fall through to literal + key-name logic (no key here so literal only)
    infer_php_literal_schema(trimmed)
}

/// Resolve the schema for `$this->propName->methodName(...)`.
/// Steps:
///  1. Find `propName`'s type from controller property declarations.
///  2. Resolve the service/class file.
///  3. Extract the method's `@return` annotation.
///  4. If `@return` is plain `array`, trace the method body for entity type.
fn resolve_this_prop_method_schema(
    ctrl_src: &str,
    prop_name: &str,
    method_name: &str,
    repo_path: &Path,
) -> Option<Value> {
    let class_name = resolve_this_property_class(ctrl_src, prop_name)?;
    let class_file = resolve_service_file(repo_path, &class_name, ctrl_src)?;
    let class_src = std::fs::read_to_string(&class_file).ok()?;

    let return_type = extract_method_return_annotation(&class_src, method_name);

    // If we have a specific typed return, use it
    if let Some(ref rt) = return_type
        && rt != "array"
        && rt != "mixed"
        && !rt.is_empty()
    {
        return schema_from_return_type(rt, repo_path, &class_src);
    }

    // Plain `@return array` or missing — trace the method body for entity type
    let method_body = extract_method_body(&class_src, method_name)?;
    if let Some(entity_schema) = find_entity_array_in_body(method_body, &class_src, repo_path, 3) {
        return Some(entity_schema);
    }

    // Last resort: name-based schema or generic array
    return_type.map(|_| json!({ "type": "array", "items": {} }))
}

/// Find the PHP class name for `$this->propName` from controller property declarations.
/// Looks for patterns like `private ClassName $propName` or `protected ?ClassName $propName`.
fn resolve_this_property_class(ctrl_src: &str, prop_name: &str) -> Option<String> {
    let search = format!("${prop_name}");
    let mut search_start = 0;
    while let Some(pos) = ctrl_src[search_start..].find(&search) {
        let abs_pos = search_start + pos;
        // Must be followed by a word boundary (space, ;, =, )
        let after = ctrl_src[abs_pos + search.len()..]
            .chars()
            .next()
            .unwrap_or(' ');
        if after.is_alphanumeric() || after == '_' {
            search_start = abs_pos + 1;
            continue;
        }

        let before = &ctrl_src[..abs_pos];
        let last_line = before.lines().last().unwrap_or("").trim();
        // e.g. `private PostPurchaseService $propName`
        let words: Vec<&str> = last_line.split_whitespace().collect();
        if words.len() >= 2 {
            let type_word = words[words.len() - 2].trim_start_matches('?');
            if type_word
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
                && type_word
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '\\')
            {
                let simple = type_word.split('\\').next_back().unwrap_or(type_word);
                return Some(simple.to_string());
            }
        }
        search_start = abs_pos + 1;
    }
    None
}

/// Scan a method body for array-returning model call patterns and trace to entity schema.
/// Returns `{ type: array, items: <entity schema> }` when found.
/// `depth` limits recursive `$this->method()` following to avoid loops.
fn find_entity_array_in_body(body: &str, src: &str, repo_path: &Path, depth: u8) -> Option<Value> {
    if depth == 0 {
        return None;
    }

    // Pattern A: ModelClass::findAll/findBy/paginate/all/getAll(
    let list_methods = [
        "::findAll(",
        "::findBy(",
        "::paginate(",
        "::getAll(",
        "::all(",
        "::get(",
    ];
    for pat in &list_methods {
        if let Some(pos) = body.find(pat) {
            let before = &body[..pos];
            let class_start = before
                .rfind(|c: char| !c.is_alphanumeric() && c != '_' && c != '\\')
                .map(|i| i + 1)
                .unwrap_or(0);
            let model_class = before[class_start..].trim();
            if model_class.is_empty() || model_class.starts_with('$') {
                continue;
            }
            let model_simple = model_class.split('\\').next_back().unwrap_or(model_class);
            if let Some((entity_class, entity_file)) =
                resolve_model_to_entity(model_simple, src, repo_path)
                && let Ok(entity_src) = std::fs::read_to_string(&entity_file)
            {
                let entity_schema = extract_casts_schema(&entity_src).or_else(|| {
                    let s = extract_payload_schema(&entity_src);
                    let has_props = s
                        .get("properties")
                        .and_then(|p| p.as_object())
                        .map(|p| !p.is_empty())
                        .unwrap_or(false);
                    if has_props { Some(s) } else { None }
                });
                if let Some(entity_schema) = entity_schema {
                    let _ = entity_class;
                    return Some(json!({ "type": "array", "items": entity_schema }));
                }
            }
        }
    }

    // Pattern B: $this->methodName() — recurse into that method
    if let Some(pos) = body.rfind("$this->") {
        let after = &body[pos + "$this->".len()..];
        let end = after
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after.len());
        let inner_method = &after[..end];
        if !inner_method.is_empty()
            && let Some(inner_body) = extract_method_body(src, inner_method)
            && let Some(schema) = find_entity_array_in_body(inner_body, src, repo_path, depth - 1)
        {
            return Some(schema);
        }
    }

    None
}

/// Try to resolve a model class to its entity via `$returnType = EntityClass::class`.
fn resolve_model_to_entity(
    model_class: &str,
    ctrl_src: &str,
    repo_path: &Path,
) -> Option<(String, std::path::PathBuf)> {
    let model_file = resolve_service_file(repo_path, model_class, ctrl_src)?;
    let model_src = std::fs::read_to_string(&model_file).ok()?;
    let full_entity = extract_return_type(&model_src)?;
    let entity_class = full_entity
        .split('\\')
        .next_back()
        .unwrap_or(&full_entity)
        .to_string();
    let entity_file = resolve_class_file(repo_path, &full_entity, &model_src)?;
    Some((entity_class, entity_file))
}

// ── Pass 6 helpers ────────────────────────────────────────────────────────────

/// Top-level resolver for Pass 6: extract the data expression from
/// `sendResponse(data: EXPR)` and attempt to infer a schema from it.
fn resolve_send_response_schema(
    body: &str,
    ctrl_src: &str,
    repo_path: &Path,
) -> Option<(Value, String)> {
    let mut search = body;
    while let Some(rel) = search.find("sendResponse(") {
        let after_sr = &search[rel + "sendResponse(".len()..];
        if let Some(data_rel) = after_sr.find("data:") {
            let depth: i32 = after_sr[..data_rel]
                .chars()
                .map(|c| match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                })
                .sum();
            if depth >= 0 {
                let after_data = after_sr[data_rel + "data:".len()..].trim_start();
                if let Some(rest) = after_data.strip_prefix('$') {
                    let var_end = rest
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(rest.len());
                    let var_name = &rest[..var_end];
                    if var_name.is_empty() {
                        search = &search[rel + 1..];
                        continue;
                    }
                    let after_var = rest[var_end..].trim_start();

                    // Skip ->toArray() — handled by Pass 5
                    if after_var.starts_with("->toArray(") {
                        search = &search[rel + 1..];
                        continue;
                    }

                    // Sub-case A: $obj->method() inline
                    if let Some(rest) = after_var.strip_prefix("->") {
                        let after_arrow = rest.trim_start();
                        let method_end = after_arrow
                            .find(|c: char| !c.is_alphanumeric() && c != '_')
                            .unwrap_or(after_arrow.len());
                        let method = &after_arrow[..method_end];
                        if !method.is_empty()
                            && let Some(class_name) =
                                resolve_var_class(body, var_name, ctrl_src, repo_path)
                            && let Some(result) =
                                resolve_method_schema(repo_path, &class_name, method, ctrl_src)
                        {
                            return Some(result);
                        }
                        search = &search[rel + 1..];
                        continue;
                    }

                    // Sub-case B: plain $var — trace its assignment
                    if let Some(result) = trace_var_assignment(body, ctrl_src, var_name, repo_path)
                    {
                        return Some(result);
                    }
                }
            }
        }
        search = &search[rel + 1..];
    }
    None
}

/// Try to find the concrete class name of a local variable `$var_name`.
/// Handles: `$var = new ClassName()`, `$var = model(ClassName::class)`.
fn resolve_var_class(
    body: &str,
    var_name: &str,
    ctrl_src: &str,
    repo_path: &Path,
) -> Option<String> {
    // Pattern: $var = new ClassName(
    let new_pat = format!("${var_name} = new ");
    if let Some(pos) = body.find(&new_pat) {
        let after = &body[pos + new_pat.len()..];
        let end = after
            .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '\\')
            .unwrap_or(after.len());
        let class = &after[..end];
        if !class.is_empty() {
            return Some(class.split('\\').next_back().unwrap_or(class).to_string());
        }
    }
    // Pattern: $var = model(ClassName::class)
    let model_pat = format!("${var_name} = model(");
    if let Some(pos) = body.find(&model_pat) {
        let after = &body[pos + model_pat.len()..];
        let end = after.find("::class").unwrap_or(0);
        if end > 0 {
            let class = after[..end].trim().trim_start_matches('\\');
            if !class.is_empty() {
                return Some(class.split('\\').next_back().unwrap_or(class).to_string());
            }
        }
    }
    // Pattern: $var = $this->someService — look for use statement of the property type
    let prop_pat = format!("${var_name} = $this->");
    if let Some(pos) = body.find(&prop_pat) {
        let after = &body[pos + prop_pat.len()..];
        let end = after
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after.len());
        let prop_name = &after[..end];
        if !prop_name.is_empty() {
            // Try to find `private|protected TypeHint $prop_name` in ctrl_src
            let typed_pat = format!("${prop_name}");
            if let Some(decl_pos) = ctrl_src.find(&typed_pat) {
                let before = &ctrl_src[..decl_pos];
                let last_line = before.lines().last().unwrap_or("").trim();
                // e.g. `private SomeService $propName`
                let words: Vec<&str> = last_line.split_whitespace().collect();
                if words.len() >= 2 {
                    let type_word = words[words.len() - 2].trim_start_matches('?');
                    if type_word
                        .chars()
                        .next()
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false)
                    {
                        // Check if this class file exists
                        if resolve_service_file(repo_path, type_word, ctrl_src).is_some() {
                            return Some(type_word.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Trace `$var_name = EXPR` in the method body and resolve a schema from EXPR.
fn trace_var_assignment(
    body: &str,
    ctrl_src: &str,
    var_name: &str,
    repo_path: &Path,
) -> Option<(Value, String)> {
    let assign_pat = format!("${var_name} =");
    // Find last assignment to avoid picking up previous iterations' assignments
    let pos = body.rfind(&assign_pat)?;
    let rhs = body[pos + assign_pat.len()..].trim_start();

    // json_decode(file_get_contents(APPPATH . 'path.json'))
    if rhs.starts_with("json_decode(") && rhs.contains("APPPATH") {
        return trace_json_data_file(rhs, repo_path);
    }

    // ClassName::staticMethod(...)
    if let Some((class_name, method)) = parse_static_call(rhs)
        && let Some(result) = resolve_method_schema(repo_path, &class_name, &method, ctrl_src)
    {
        return Some(result);
    }

    // (new ClassName(...))->method(...)
    if let Some((class_name, method)) = parse_new_instance_call(rhs)
        && let Some(result) = resolve_method_schema(repo_path, &class_name, &method, ctrl_src)
    {
        return Some(result);
    }

    // $otherVar->method(...)
    if let Some(rest) = rhs.strip_prefix('$') {
        let var_end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let other_var = &rest[..var_end];
        let after = rest[var_end..].trim_start();
        if let Some(rest) = after.strip_prefix("->") {
            let after_arrow = rest.trim_start();
            let method_end = after_arrow
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(after_arrow.len());
            let method = &after_arrow[..method_end];
            if !method.is_empty()
                && method != "toArray"
                && let Some(class_name) = resolve_var_class(body, other_var, ctrl_src, repo_path)
                && let Some(result) =
                    resolve_method_schema(repo_path, &class_name, method, ctrl_src)
            {
                return Some(result);
            }
        }
    }

    None
}

/// Parse `ClassName::method(` from the start of `rhs`. Returns (ClassName, method).
fn parse_static_call(rhs: &str) -> Option<(String, String)> {
    let cc_pos = rhs.find("::")?;
    let class_part = rhs[..cc_pos].trim();
    if class_part.starts_with('$') || class_part.is_empty() {
        return None;
    }
    if !class_part
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '\\')
    {
        return None;
    }

    let after_cc = rhs[cc_pos + 2..].trim_start();
    let method_end = after_cc
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(after_cc.len());
    let method = &after_cc[..method_end];
    if method.is_empty() || method == "class" {
        return None;
    }

    let class_simple = class_part
        .split('\\')
        .next_back()
        .unwrap_or(class_part)
        .to_string();
    Some((class_simple, method.to_string()))
}

/// Parse `(new ClassName(...))->method(` from the start of `rhs`.
fn parse_new_instance_call(rhs: &str) -> Option<(String, String)> {
    let trimmed = rhs.trim_start();
    if !trimmed.starts_with("(new ") {
        return None;
    }

    let outer_inner = extract_balanced(trimmed, '(', ')')?;
    // outer_inner = "new ClassName(args)"
    let new_inner = outer_inner.trim_start_matches("new ").trim_start();
    let class_end = new_inner
        .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '\\')
        .unwrap_or(new_inner.len());
    let class_name = &new_inner[..class_end];
    if class_name.is_empty() {
        return None;
    }

    let after_outer = &trimmed[outer_inner.len() + 2..].trim_start();
    if !after_outer.starts_with("->") {
        return None;
    }
    let after_arrow = after_outer[2..].trim_start();
    let method_end = after_arrow
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(after_arrow.len());
    let method = &after_arrow[..method_end];
    if method.is_empty() {
        return None;
    }

    let class_simple = class_name
        .split('\\')
        .next_back()
        .unwrap_or(class_name)
        .to_string();
    Some((class_simple, method.to_string()))
}

/// Find a service/library/model class file — checks use statements first,
/// then common CI4 app directories.
fn resolve_service_file(repo_path: &Path, class_name: &str, ctrl_src: &str) -> Option<PathBuf> {
    if let Some(p) = resolve_class_file(repo_path, class_name, ctrl_src) {
        return Some(p);
    }
    for dir in &["Services", "Libraries", "Models", "Repositories", "Helpers"] {
        let p = repo_path
            .join("apps/ci4/app")
            .join(dir)
            .join(format!("{class_name}.php"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Resolve `class_name::method_name` to a JSON schema by reading the `@return`
/// docblock annotation from the class file. Returns (schema, schema_name).
fn resolve_method_schema(
    repo_path: &Path,
    class_name: &str,
    method_name: &str,
    ctrl_src: &str,
) -> Option<(Value, String)> {
    let class_file = resolve_service_file(repo_path, class_name, ctrl_src)?;
    let class_src = std::fs::read_to_string(&class_file).ok()?;
    let return_type = extract_method_return_annotation(&class_src, method_name)?;
    let schema = schema_from_return_type(&return_type, repo_path, &class_src)?;
    let name = format!("{class_name}{}", php_capitalize(method_name));
    Some((schema, name))
}

/// Read the `@return` annotation from the docblock immediately above `function method_name(`.
fn extract_method_return_annotation(class_src: &str, method_name: &str) -> Option<String> {
    let needle = format!("function {}(", method_name);
    let method_pos = class_src.find(&needle)?;
    let before = &class_src[..method_pos];
    let docblock_end = before.rfind("*/")?;
    let docblock_start = before[..docblock_end].rfind("/**")?;
    let docblock = &before[docblock_start..docblock_end + 2];

    for line in docblock.lines() {
        let trimmed = line.trim().trim_start_matches('*').trim();
        if let Some(rest) = trimmed.strip_prefix("@return ") {
            let type_str = rest.split_whitespace().next().unwrap_or("").trim();
            if !type_str.is_empty()
                && type_str != "void"
                && type_str != "null"
                && type_str != "mixed"
            {
                return Some(type_str.to_string());
            }
        }
    }
    None
}

/// Convert a PHP `@return` type string to a JSON Schema value.
fn schema_from_return_type(return_type: &str, repo_path: &Path, class_src: &str) -> Option<Value> {
    // Type[] → array with typed items
    if return_type.ends_with("[]") {
        let inner = return_type.trim_end_matches("[]");
        let item_schema = match php_type_to_json(inner) {
            "object" => {
                // Try to resolve and introspect the item class
                resolve_service_file(repo_path, inner, class_src)
                    .and_then(|f| std::fs::read_to_string(&f).ok())
                    .map(|src| {
                        let s = extract_payload_schema(&src);
                        if s.get("properties")
                            .and_then(|p| p.as_object())
                            .map(|p| !p.is_empty())
                            .unwrap_or(false)
                        {
                            s
                        } else {
                            json!({ "type": "object", "description": inner })
                        }
                    })
                    .unwrap_or_else(|| json!({ "type": "object", "description": inner }))
            }
            t => json!({ "type": t }),
        };
        return Some(json!({ "type": "array", "items": item_schema }));
    }

    // Collection<Type> or array<Type>
    if let Some(inner) = return_type
        .strip_prefix("array<")
        .and_then(|s| s.strip_suffix('>'))
        .or_else(|| {
            return_type
                .strip_prefix("Collection<")
                .and_then(|s| s.strip_suffix('>'))
        })
    {
        let item_schema = match php_type_to_json(inner) {
            "object" => json!({ "type": "object", "description": inner }),
            t => json!({ "type": t }),
        };
        return Some(json!({ "type": "array", "items": item_schema }));
    }

    match php_type_to_json(return_type) {
        "array" => Some(json!({ "type": "array", "items": {} })),
        "object" => {
            // Try to resolve and introspect the returned class
            let schema = resolve_service_file(repo_path, return_type, class_src)
                .and_then(|f| std::fs::read_to_string(&f).ok())
                .map(|src| {
                    let s = extract_payload_schema(&src);
                    if s.get("properties")
                        .and_then(|p| p.as_object())
                        .map(|p| !p.is_empty())
                        .unwrap_or(false)
                    {
                        s
                    } else {
                        json!({ "type": "object", "description": return_type })
                    }
                })
                .unwrap_or_else(|| json!({ "type": "object", "description": return_type }));
            Some(schema)
        }
        t => Some(json!({ "type": t })),
    }
}

/// Read a JSON data file referenced by `json_decode(file_get_contents(APPPATH . 'path.json'))`
/// and infer a schema from its structure.
fn trace_json_data_file(rhs: &str, repo_path: &Path) -> Option<(Value, String)> {
    let apppath_pos = rhs.find("APPPATH")?;
    let after = rhs[apppath_pos + "APPPATH".len()..].trim_start();
    let after_dot = after.trim_start_matches('.').trim_start();
    if !after_dot.starts_with(['\'', '"']) {
        return None;
    }
    let q = after_dot.as_bytes()[0] as char;
    let qend = find_closing_quote(&after_dot[1..], q)?;
    let rel_path = &after_dot[1..qend + 1];

    let json_path = repo_path
        .join("apps/ci4/app")
        .join(rel_path.trim_start_matches('/'));
    if !json_path.exists() {
        return None;
    }
    let json_src = std::fs::read_to_string(&json_path).ok()?;
    let json_val: Value = serde_json::from_str(&json_src).ok()?;

    let schema = infer_schema_from_json(&json_val);
    let name = rel_path
        .trim_end_matches(".json")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(php_capitalize)
        .collect::<Vec<_>>()
        .join("");
    let name = if name.is_empty() {
        "JsonData".to_string()
    } else {
        name
    };
    Some((schema, name))
}

/// Recursively infer a JSON Schema from a concrete JSON value.
fn infer_schema_from_json(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let mut props = serde_json::Map::new();
            for (k, v) in map {
                props.insert(k.clone(), infer_schema_from_json(v));
            }
            json!({ "type": "object", "properties": Value::Object(props) })
        }
        Value::Array(arr) => {
            let item_schema = arr.first().map(infer_schema_from_json).unwrap_or(json!({}));
            json!({ "type": "array", "items": item_schema })
        }
        Value::String(_) => json!({ "type": "string" }),
        Value::Number(n) => {
            if n.is_f64() {
                json!({ "type": "number" })
            } else {
                json!({ "type": "integer" })
            }
        }
        Value::Bool(_) => json!({ "type": "boolean" }),
        Value::Null => json!({ "type": "null" }),
    }
}

/// Capitalize the first letter of a string.
fn php_capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci4_path_num_param() {
        assert_eq!(
            ci4_path_to_oas("admin/api/v1/shop/(:num)"),
            "/admin/api/v1/shop/{id}"
        );
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
        assert_eq!(
            schema_component_name("V1/Shop/PutTheme.json"),
            "V1ShopPutTheme"
        );
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
