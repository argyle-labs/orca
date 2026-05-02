//! PHP source analysis via tree-sitter-php.
//!
//! Wraps the tree-sitter PHP grammar to provide typed extraction functions
//! for the patterns found in CI4 admin-api source code.
//!
//! ## Why tree-sitter?
//!
//! tree-sitter provides a battle-tested, error-recovering PHP grammar
//! maintained at <https://github.com/tree-sitter/tree-sitter-php>.  It is
//! the same grammar used by GitHub Copilot, Neovim, and VS Code for PHP
//! language support — a far more reliable foundation than hand-rolled string
//! scanning.
//!
//! ## Optional feature
//!
//! The scanner crate gates this module behind the `php-ast` Cargo feature.
//! Without that feature the module compiles but [`PhpFile::parse`] always
//! returns `None`, causing ci4_generator to skip AST-based passes.
//! Enable it with:
//!
//! ```toml
//! brain-scanner = { ..., features = ["php-ast"] }
//! ```

use serde_json::{Map, Value, json};

#[cfg(feature = "php-ast")]
use tree_sitter::{Node, Parser as TsParser};
#[cfg(feature = "php-ast")]
use tree_sitter_php::LANGUAGE_PHP_ONLY;

// ── PhpFile ───────────────────────────────────────────────────────────────────

/// A parsed PHP source file.
///
/// Construct via [`PhpFile::parse`]; all methods are zero-copy over the
/// original source bytes held inside this struct.
pub struct PhpFile {
    #[cfg(feature = "php-ast")]
    source: Vec<u8>,
    #[cfg(feature = "php-ast")]
    tree: tree_sitter::Tree,
}

impl PhpFile {
    /// Parse PHP source text.
    ///
    /// Returns `None` when:
    /// * the `php-ast` feature is disabled, or
    /// * tree-sitter reports a language version mismatch.
    ///
    /// Callers treat `None` as "no AST available" and fall back to whatever
    /// degraded analysis they can do without it.
    pub fn parse(src: &str) -> Option<Self> {
        #[cfg(feature = "php-ast")]
        {
            let mut parser = TsParser::new();
            parser.set_language(&LANGUAGE_PHP_ONLY.into()).ok()?;
            let tree = parser.parse(src.as_bytes(), None)?;
            return Some(PhpFile {
                source: src.as_bytes().to_vec(),
                tree,
            });
        }
        #[cfg(not(feature = "php-ast"))]
        {
            let _ = src;
            None
        }
    }

    // ── Internal text helpers ─────────────────────────────────────────────

    #[cfg(feature = "php-ast")]
    fn text_of<'s>(&'s self, node: Node<'_>) -> &'s str {
        std::str::from_utf8(&self.source[node.byte_range()]).unwrap_or("")
    }

    // ── Route extraction ──────────────────────────────────────────────────

    /// Extract all `$routes->METHOD(path, controller, options)` calls.
    ///
    /// CI4 routes are registered in `app/Config/Routes/*.php` using the
    /// RouteCollection fluent API.  Each call maps directly to one OAS path +
    /// method pair.
    pub fn route_registrations(&self) -> Vec<RouteCall> {
        #[cfg(feature = "php-ast")]
        { let tree = &self.tree;
            let mut out = Vec::new();
            self.collect_route_calls(tree.root_node(), &mut out);
            return out;
        }
        #[cfg(not(feature = "php-ast"))]
        Vec::new()
    }

    #[cfg(feature = "php-ast")]
    fn collect_route_calls<'t>(&self, node: Node<'t>, out: &mut Vec<RouteCall>) {
        if node.kind() == "member_call_expression" {
            if let Some(call) = self.try_route_call(node) {
                out.push(call);
                return; // don't recurse into what we already consumed
            }
        }
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            self.collect_route_calls(child, out);
        }
    }

    #[cfg(feature = "php-ast")]
    fn try_route_call(&self, node: Node<'_>) -> Option<RouteCall> {
        const HTTP_METHODS: &[&str] = &["get","post","put","patch","delete","options","head"];
        let obj = node.child_by_field_name("object")?;
        if self.text_of(obj) != "$routes" { return None; }

        let method_node = node.child_by_field_name("name")?;
        let method = self.text_of(method_node);
        if !HTTP_METHODS.contains(&method) { return None; }

        let args_node = node.child_by_field_name("arguments")?;
        let args = self.argument_nodes(args_node);
        if args.is_empty() { return None; }

        let path = self.string_value(args[0])?;
        let controller = args.get(1)
            .and_then(|n| self.string_value(*n))
            .map(|s| {
                // Strip capture-group suffixes: `Api\V1\Foo::get/$1` → `Api\V1\Foo::get`
                if let Some(p) = s.find("/$") { s[..p].to_string() } else { s }
            })
            .unwrap_or_default();

        let filters = args.get(2)
            .map(|n| self.extract_filter_values(*n))
            .unwrap_or_default();

        Some(RouteCall { method: method.to_string(), path, controller, filters })
    }

    #[cfg(feature = "php-ast")]
    fn argument_nodes<'t>(&self, args_node: Node<'t>) -> Vec<Node<'t>> {
        let mut cur = args_node.walk();
        args_node.children(&mut cur)
            .filter(|n| n.kind() == "argument")
            .collect()
    }

    #[cfg(feature = "php-ast")]
    fn string_value(&self, node: Node<'_>) -> Option<String> {
        // Unwrap argument wrapper
        let node = if node.kind() == "argument" {
            let mut c = node.walk();
            node.children(&mut c)
                .find(|n| n.kind() != "," && n.kind() != "comment")?
        } else { node };

        match node.kind() {
            "string" | "encapsed_string" => {
                let raw = self.text_of(node);
                // Strip surrounding single or double quotes
                Some(raw.trim_matches(['\'', '"']).to_string())
            }
            _ => None,
        }
    }

    #[cfg(feature = "php-ast")]
    fn extract_filter_values(&self, options_node: Node<'_>) -> Vec<String> {
        // options_node is the argument wrapping ['filter' => [...]]
        let mut out = Vec::new();
        self.walk_for_filter_values(options_node, &mut out);
        out
    }

    #[cfg(feature = "php-ast")]
    fn walk_for_filter_values(&self, node: Node<'_>, out: &mut Vec<String>) {
        // We're looking for an array_element_initializer whose key == "filter"
        if node.kind() == "array_element_initializer" {
            let mut c = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut c).collect();
            // Typical structure: key "=>" value (three non-whitespace children)
            if children.len() >= 3 {
                let key = self.string_value(children[0]).unwrap_or_default();
                if key == "filter" {
                    self.collect_strings(children[children.len()-1], out);
                    return;
                }
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            self.walk_for_filter_values(child, out);
        }
    }

    #[cfg(feature = "php-ast")]
    fn collect_strings(&self, node: Node<'_>, out: &mut Vec<String>) {
        if node.kind() == "string" || node.kind() == "encapsed_string" {
            if let Some(s) = self.string_value(node) {
                if !s.is_empty() { out.push(s); }
            }
            return;
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            self.collect_strings(child, out);
        }
    }

    // ── Protected property extraction ─────────────────────────────────────

    /// Extract `protected $casts = [...]` as `(snake_key, cast_type)` pairs.
    ///
    /// CI4 entities declare `$casts` to map database column names to PHP
    /// types.  We convert these into JSON Schema properties for response
    /// typing in Pass 5.
    pub fn casts_array(&self) -> Option<Vec<(String, String)>> {
        #[cfg(feature = "php-ast")]
        { let tree = &self.tree;
            return self.find_array_prop(tree.root_node(), "casts");
        }
        #[cfg(not(feature = "php-ast"))]
        None
    }

    /// Extract the value of `protected $returnType = ClassName::class`.
    ///
    /// CI4 models declare `$returnType` to specify the entity class used when
    /// hydrating query results.  Pass 5 uses this to jump from a model to its
    /// entity and from there to the `$casts` schema.
    pub fn return_type(&self) -> Option<String> {
        #[cfg(feature = "php-ast")]
        { let tree = &self.tree;
            return self.find_scalar_prop(tree.root_node(), "returnType");
        }
        #[cfg(not(feature = "php-ast"))]
        None
    }

    #[cfg(feature = "php-ast")]
    fn find_array_prop(&self, node: Node<'_>, prop: &str) -> Option<Vec<(String, String)>> {
        if self.is_protected_property(node, prop) {
            if let Some(default) = self.property_default(node, prop) {
                return Some(self.array_key_value_pairs(default));
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if let Some(r) = self.find_array_prop(child, prop) { return Some(r); }
        }
        None
    }

    #[cfg(feature = "php-ast")]
    fn find_scalar_prop(&self, node: Node<'_>, prop: &str) -> Option<String> {
        if self.is_protected_property(node, prop) {
            if let Some(default) = self.property_default(node, prop) {
                return Some(self.text_of(default).to_string());
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if let Some(r) = self.find_scalar_prop(child, prop) { return Some(r); }
        }
        None
    }

    #[cfg(feature = "php-ast")]
    fn is_protected_property(&self, node: Node<'_>, prop: &str) -> bool {
        if node.kind() != "property_declaration" { return false; }
        let has_protected = {
            let mut c = node.walk();
            node.children(&mut c).any(|n| {
                n.kind() == "visibility_modifier" && self.text_of(n) == "protected"
            })
        };
        if !has_protected { return false; }
        let has_name = {
            let mut c = node.walk();
            node.children(&mut c).any(|n| {
                if n.kind() != "property_element" { return false; }
                n.child_by_field_name("name")
                    .map(|v| self.text_of(v).trim_start_matches('$') == prop)
                    .unwrap_or(false)
            })
        };
        has_name
    }

    #[cfg(feature = "php-ast")]
    fn property_default<'t>(&self, node: Node<'t>, prop: &str) -> Option<Node<'t>> {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if child.kind() == "property_element" {
                if child.child_by_field_name("name")
                    .map(|v| self.text_of(v).trim_start_matches('$') == prop)
                    .unwrap_or(false)
                {
                    return child.child_by_field_name("default");
                }
            }
        }
        None
    }

    #[cfg(feature = "php-ast")]
    fn array_key_value_pairs(&self, node: Node<'_>) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        let mut c = node.walk();
        for item in node.children(&mut c) {
            if item.kind() != "array_element_initializer" { continue; }
            let mut ic = item.walk();
            let parts: Vec<Node<'_>> = item.children(&mut ic).collect();
            if parts.len() < 3 { continue; }
            let key = self.string_value(parts[0]).unwrap_or_default();
            let val = self.string_value(parts[parts.len()-1]).unwrap_or_default();
            if !key.is_empty() && !val.is_empty() {
                pairs.push((key, val));
            }
        }
        pairs
    }

    // ── setJSON literal response inference ────────────────────────────────

    /// Return the richest `->setJSON([...])` literal schema found in `method`.
    ///
    /// admin-api controllers frequently return `$this->response->setJSON([...])`
    /// with a literal associative array.  Extracting those keys gives us a
    /// typed response schema without needing to trace through services.
    pub fn set_json_schema(&self, method_name: &str) -> Option<Map<String, Value>> {
        #[cfg(feature = "php-ast")]
        { let tree = &self.tree;
            let method = self.find_method(tree.root_node(), method_name)?;
            let mut best: Map<String, Value> = Map::new();
            self.collect_set_json(method, &mut best);
            return if best.is_empty() { None } else { Some(best) };
        }
        #[cfg(not(feature = "php-ast"))]
        { let _ = method_name; None }
    }

    #[cfg(feature = "php-ast")]
    fn collect_set_json(&self, node: Node<'_>, best: &mut Map<String, Value>) {
        if node.kind() == "member_call_expression" {
            if let Some(nm) = node.child_by_field_name("name") {
                let n = self.text_of(nm);
                if n == "setJSON" || n == "setJson" {
                    if let Some(args) = node.child_by_field_name("arguments") {
                        let a = self.argument_nodes(args);
                        if let Some(first) = a.first() {
                            let props = self.array_to_schema(*first);
                            if props.len() > best.len() { *best = props; }
                        }
                    }
                }
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            self.collect_set_json(child, best);
        }
    }

    #[cfg(feature = "php-ast")]
    fn array_to_schema(&self, node: Node<'_>) -> Map<String, Value> {
        // Unwrap argument wrapper
        let node = if node.kind() == "argument" {
            let mut c = node.walk();
            match node.children(&mut c).find(|n| n.kind() != "," && n.kind() != "comment") {
                Some(n) => n,
                None => return Map::new(),
            }
        } else { node };

        if node.kind() != "array_creation_expression" { return Map::new(); }

        let mut props = Map::new();
        let mut c = node.walk();
        for item in node.children(&mut c) {
            if item.kind() != "array_element_initializer" { continue; }
            let mut ic = item.walk();
            let parts: Vec<Node<'_>> = item.children(&mut ic).collect();
            if parts.len() < 3 { continue; }
            let key = self.string_value(parts[0]).unwrap_or_default();
            if key.is_empty() { continue; }
            props.insert(key, node_to_json_schema(parts[parts.len()-1]));
        }
        props
    }

    // ── sendResponse(data: $var->toArray()) ───────────────────────────────

    /// Find `sendResponse(data: $var->toArray())` in a method body and return
    /// the variable name (`var`).
    ///
    /// This is the entry point for the 3-hop entity trace in Pass 5:
    /// `$var` → `$model->findX()` → `new ModelClass()` → `$returnType` → entity.
    pub fn send_response_to_array_var(&self, method_name: &str) -> Option<String> {
        #[cfg(feature = "php-ast")]
        { let tree = &self.tree;
            let method = self.find_method(tree.root_node(), method_name)?;
            return self.find_to_array_var(method);
        }
        #[cfg(not(feature = "php-ast"))]
        { let _ = method_name; None }
    }

    #[cfg(feature = "php-ast")]
    fn find_to_array_var(&self, node: Node<'_>) -> Option<String> {
        if node.kind() == "member_call_expression" || node.kind() == "function_call_expression" {
            if let Some(name) = node.child_by_field_name("name") {
                if self.text_of(name) == "sendResponse" {
                    if let Some(args) = node.child_by_field_name("arguments") {
                        return self.data_to_array_var(args);
                    }
                }
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if let Some(v) = self.find_to_array_var(child) { return Some(v); }
        }
        None
    }

    #[cfg(feature = "php-ast")]
    fn data_to_array_var(&self, args_node: Node<'_>) -> Option<String> {
        let mut c = args_node.walk();
        for arg in args_node.children(&mut c) {
            if arg.kind() != "named_argument" { continue; }
            let name = arg.child_by_field_name("name")?;
            if self.text_of(name) != "data" { continue; }
            let value = arg.child_by_field_name("value")?;
            // value should be `$var->toArray()`
            if value.kind() == "member_call_expression" {
                if let Some(nm) = value.child_by_field_name("name") {
                    if self.text_of(nm) == "toArray" {
                        if let Some(obj) = value.child_by_field_name("object") {
                            if obj.kind() == "variable_name" {
                                return Some(self.text_of(obj).trim_start_matches('$').to_string());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    #[cfg(feature = "php-ast")]
    fn find_method<'t>(&self, node: Node<'t>, method_name: &str) -> Option<Node<'t>> {
        if node.kind() == "method_declaration" {
            if let Some(name) = node.child_by_field_name("name") {
                if self.text_of(name) == method_name { return Some(node); }
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if let Some(r) = self.find_method(child, method_name) { return Some(r); }
        }
        None
    }
}

// ── Free functions shared across generators ───────────────────────────────────

/// Map a tree-sitter PHP literal node kind to a JSON Schema type fragment.
/// Variables and expressions not resolvable at parse time emit `{}`.
#[cfg(feature = "php-ast")]
fn node_to_json_schema(node: Node<'_>) -> Value {
    match node.kind() {
        "boolean" => json!({ "type": "boolean" }),
        "integer" => json!({ "type": "integer" }),
        "float" => json!({ "type": "number" }),
        "string" | "encapsed_string" => json!({ "type": "string" }),
        "null" => json!({ "type": "null" }),
        "array_creation_expression" => json!({ "type": "array" }),
        _ => json!({}),
    }
}

/// Convert a CI4 `$casts` type string (e.g. `"?int"`, `"?json-array"`) to
/// a JSON Schema fragment.  Nullable types produce `["T","null"]` union arrays.
pub fn ci4_cast_to_json_schema(cast: &str) -> Value {
    let nullable = cast.starts_with('?');
    let base = if nullable { &cast[1..] } else { cast };
    let (t, fmt): (&str, Option<&str>) = match base {
        "int" | "integer"     => ("integer", None),
        "string"              => ("string",  None),
        "bool" | "boolean"    => ("boolean", None),
        "float" | "double"    => ("number",  None),
        "datetime"            => ("string",  Some("date-time")),
        "json" | "json-array" => ("object",  None),
        "array"               => ("array",   None),
        _                     => ("string",  None),
    };
    if nullable {
        match fmt {
            Some(f) => json!({ "type": [t, "null"], "format": f }),
            None    => json!({ "type": [t, "null"] }),
        }
    } else {
        match fmt {
            Some(f) => json!({ "type": t, "format": f }),
            None    => json!({ "type": t }),
        }
    }
}

/// snake_case → camelCase for CI4 entity field names (`start_date` → `startDate`).
pub fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if c == '_' { upper = true; }
        else if upper { out.extend(c.to_uppercase()); upper = false; }
        else { out.push(c); }
    }
    out
}

// ── RouteCall ─────────────────────────────────────────────────────────────────

/// A single CI4 route registration extracted from a routes file.
#[derive(Debug, Clone)]
pub struct RouteCall {
    pub method: String,
    pub controller: String,
    pub filters: Vec<String>,
    pub path: String,
}
