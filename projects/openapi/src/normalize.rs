//! Reusable preprocessors that bring upstream OpenAPI specs in line with
//! what `progenitor` can codegen. Real-world specs (Sonarr, Radarr, Jellyfin,
//! …) routinely miss `operationId`, mix multipart bodies, or list multiple
//! response media types per status — all of which progenitor rejects.
//!
//! Each fix lives here so a new consumer crate only writes a thin build.rs:
//!
//! ```ignore
//! let mut spec: openapiv3::OpenAPI = serde_json::from_str(&raw)?;
//! openapi::normalize::for_progenitor(&mut spec);
//! let tokens = progenitor::Generator::default().generate_tokens(&spec)?;
//! ```
//!
//! Add new preprocessors here as we discover more upstream-spec edge cases;
//! never patch them in a single integration's build.rs.

use openapiv3::{
    MediaType, OpenAPI, Operation, Parameter, QueryStyle, ReferenceOr, Schema, SchemaData,
    SchemaKind, StatusCode, StringFormat, StringType, Type, VariantOrUnknownOrEmpty,
};

/// What `for_progenitor` had to change. Surfaced so consumer build scripts
/// can `cargo:warning=` each entry — that way a new upstream spec version
/// adding a multipart endpoint (or any other normalization hit) shows up in
/// the build log instead of silently disappearing from the generated client.
#[derive(Debug, Default, Clone)]
pub struct NormalizeReport {
    /// Synthesized operationIds: `(method, path, generated_id)`.
    pub synthesized_ids: Vec<(String, String, String)>,
    /// Operations whose multipart request body was rewritten to
    /// `application/octet-stream` (raw bytes) so progenitor can codegen them.
    /// Callers must assemble the multipart body themselves (e.g. via
    /// `reqwest::multipart::Form` → bytes) before invoking the generated fn.
    pub rewrote_multipart: Vec<String>,
    /// Request bodies whose alternate media types were collapsed away.
    /// `(op_label, kept, dropped)`.
    pub collapsed_requests: Vec<(String, String, Vec<String>)>,
    /// Responses whose alternate media types were collapsed away.
    /// `(op_label + status, kept, dropped)`.
    pub collapsed_responses: Vec<(String, String, Vec<String>)>,
    /// Operations whose multiple 2xx responses had their schemas merged
    /// into a synthetic `oneOf` covering every distinct response shape, so
    /// progenitor emits a single sum-type return for all success statuses.
    /// All status codes stay routable; every response shape stays
    /// callable as a variant of the generated enum. Empty-body 2xx
    /// responses contribute a `null`-typed variant.
    /// `(op_label, statuses, variant_count)`.
    pub merged_success_responses: Vec<(String, Vec<String>, usize)>,
    /// Same as `merged_success_responses` but for the error bucket
    /// (4xx/5xx + `default`). Progenitor's assertion fires there too when
    /// schemas diverge across error statuses.
    pub merged_error_responses: Vec<(String, Vec<String>, usize)>,
    /// Query parameters whose `style: deepObject` was rewritten to `form`.
    /// progenitor only codegens `form`-style query params; it rejects every
    /// other style outright (it does *not* care whether the schema is an
    /// object). Rewriting the style to `form` keeps the parameter — with its
    /// full object type — in the generated client rather than dropping it.
    /// `(op_label, param_name)`.
    pub rewrote_deepobject_params: Vec<(String, String)>,
}

impl NormalizeReport {
    /// Emit `cargo:warning=` lines so each item appears in the build log.
    /// Intended for use from a consumer's build.rs.
    pub fn emit_cargo_warnings(&self, crate_name: &str) {
        for op in &self.rewrote_multipart {
            println!(
                "cargo:warning={crate_name}: rewrote multipart op {op} -> application/octet-stream (caller assembles body)"
            );
        }
        for (op, kept, dropped) in &self.collapsed_requests {
            println!(
                "cargo:warning={crate_name}: collapsed request {op} kept={kept} dropped={dropped:?}"
            );
        }
        for (op, kept, dropped) in &self.collapsed_responses {
            println!(
                "cargo:warning={crate_name}: collapsed response {op} kept={kept} dropped={dropped:?}"
            );
        }
        for (op, statuses, variants) in &self.merged_success_responses {
            println!(
                "cargo:warning={crate_name}: merged success responses {op} statuses={statuses:?} into oneOf with {variants} variant(s)"
            );
        }
        for (op, statuses, variants) in &self.merged_error_responses {
            println!(
                "cargo:warning={crate_name}: merged error responses {op} statuses={statuses:?} into oneOf with {variants} variant(s)"
            );
        }
        for (op, name) in &self.rewrote_deepobject_params {
            println!(
                "cargo:warning={crate_name}: rewrote query param {name} on {op} style deepObject -> form (progenitor only codegens form)"
            );
        }
    }
}

/// Run the full preprocessor chain that maps "imperfect but valid OpenAPI"
/// to "what progenitor accepts." Idempotent. Returns a report of every
/// change made — `()`-discard if you don't care.
pub fn for_progenitor(spec: &mut OpenAPI) -> NormalizeReport {
    let mut r = NormalizeReport::default();
    synthesize_operation_ids(spec, &mut r);
    rewrite_multipart_to_octet_stream(spec, &mut r);
    collapse_response_media_types(spec, &mut r);
    collapse_request_media_types(spec, &mut r);
    merge_success_response_schemas(spec, &mut r);
    merge_error_response_schemas(spec, &mut r);
    rewrite_deepobject_query_params(spec, &mut r);
    r
}

/// Rewrite query parameters declared with `style: deepObject` to `style: form`.
///
/// progenitor's method codegen accepts *only* `QueryStyle::Form` for query
/// parameters and returns `unsupported style of query parameter` for any
/// other style — regardless of the parameter's schema. `deepObject` is the
/// common style for object-valued query params (e.g. Plex's `prefs`, `hints`).
/// Rewriting the style to `form` keeps the parameter, with its full object
/// type, in the generated client instead of forcing it to be dropped — the
/// wire serialization progenitor emits for the object is form-style, which is
/// the only encoding it supports anyway.
///
/// Parameters can live on the path item (shared across methods) or on the
/// individual operation; both are handled.
pub fn rewrite_deepobject_query_params(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    fn fix(params: &mut [ReferenceOr<Parameter>], label: &str, report: &mut NormalizeReport) {
        for p in params.iter_mut() {
            if let ReferenceOr::Item(Parameter::Query {
                parameter_data,
                style,
                ..
            }) = p
                && matches!(style, QueryStyle::DeepObject)
            {
                *style = QueryStyle::Form;
                report
                    .rewrote_deepobject_params
                    .push((label.to_string(), parameter_data.name.clone()));
            }
        }
    }

    for (path, item) in spec.paths.paths.iter_mut() {
        let ReferenceOr::Item(item) = item else {
            continue;
        };
        fix(&mut item.parameters, path, report);
        for (method, op) in [
            ("get", &mut item.get),
            ("put", &mut item.put),
            ("post", &mut item.post),
            ("delete", &mut item.delete),
            ("options", &mut item.options),
            ("head", &mut item.head),
            ("patch", &mut item.patch),
            ("trace", &mut item.trace),
        ] {
            if let Some(op) = op {
                let label = format!("{method} {path}");
                fix(&mut op.parameters, &label, report);
            }
        }
    }
}

fn for_each_op_mut(spec: &mut OpenAPI, mut f: impl FnMut(&str, &str, &mut Option<Operation>)) {
    for (path, item) in spec.paths.paths.iter_mut() {
        let ReferenceOr::Item(item) = item else {
            continue;
        };
        for (method, op) in [
            ("get", &mut item.get),
            ("put", &mut item.put),
            ("post", &mut item.post),
            ("delete", &mut item.delete),
            ("options", &mut item.options),
            ("head", &mut item.head),
            ("patch", &mut item.patch),
            ("trace", &mut item.trace),
        ] {
            f(method, path, op);
        }
    }
}

/// Synthesize a stable `operationId` (`{method}_{slugified_path}`) for any
/// operation that doesn't already have one. Progenitor uses operationId as
/// the function name on the generated `Client`, so deterministic naming
/// matters: the same spec across builds → the same client API.
pub fn synthesize_operation_ids(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    for_each_op_mut(spec, |method, path, op| {
        if let Some(op) = op
            && op.operation_id.is_none()
        {
            let id = synth_id(method, path);
            report
                .synthesized_ids
                .push((method.to_string(), path.to_string(), id.clone()));
            op.operation_id = Some(id);
        }
    });
}

/// Rewrite any `multipart/*` request body to a single
/// `application/octet-stream` entry with `format: binary`. Progenitor can't
/// codegen multipart, but it *can* codegen an op that takes raw bytes —
/// callers (e.g. Sonarr `POST /login`) build the multipart body themselves
/// via `reqwest::multipart::Form`, serialize to bytes, and pass through. The
/// operation stays reachable from the generated client, which is the whole
/// point: dropping `/login` would block login automation.
pub fn rewrite_multipart_to_octet_stream(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    for_each_op_mut(spec, |method, path, op| {
        let Some(o) = op.as_mut() else { return };
        let Some(ReferenceOr::Item(body)) = o.request_body.as_mut() else {
            return;
        };
        if !body.content.keys().any(|k| k.starts_with("multipart/")) {
            return;
        }
        body.content.clear();
        body.content
            .insert("application/octet-stream".into(), octet_stream_media_type());
        report
            .rewrote_multipart
            .push(format!("{} {}", method.to_uppercase(), path));
    });
}

fn octet_stream_media_type() -> MediaType {
    MediaType {
        schema: Some(ReferenceOr::Item(Schema {
            schema_data: SchemaData::default(),
            schema_kind: SchemaKind::Type(Type::String(StringType {
                format: VariantOrUnknownOrEmpty::Item(StringFormat::Binary),
                ..Default::default()
            })),
        })),
        ..Default::default()
    }
}

/// Progenitor errors with "more media types than expected" when a response
/// (or request) lists more than one media type. Real specs commonly serve
/// the same payload as `application/json` + `text/json` + `application/*+json`
/// — orca only ever wants the JSON one. Keep the first JSON-ish entry and
/// drop the rest.
pub fn collapse_response_media_types(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    let mut hits: Vec<(String, String, Vec<String>)> = Vec::new();
    for_each_op_mut(spec, |method, path, op| {
        let Some(op) = op else { return };
        let label = format!("{} {}", method.to_uppercase(), path);
        for (status, resp) in op.responses.responses.iter_mut() {
            if let ReferenceOr::Item(r) = resp
                && let Some((kept, dropped)) = keep_one_json_media_type(&mut r.content)
            {
                hits.push((format!("{label} -> {status:?}"), kept, dropped));
            }
        }
        if let Some(ReferenceOr::Item(r)) = op.responses.default.as_mut()
            && let Some((kept, dropped)) = keep_one_json_media_type(&mut r.content)
        {
            hits.push((format!("{label} -> default"), kept, dropped));
        }
    });
    report.collapsed_responses.extend(hits);
}

/// Same idea as `collapse_response_media_types`, applied to request bodies.
pub fn collapse_request_media_types(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    let mut hits: Vec<(String, String, Vec<String>)> = Vec::new();
    for_each_op_mut(spec, |method, path, op| {
        if let Some(op) = op
            && let Some(ReferenceOr::Item(body)) = op.request_body.as_mut()
            && let Some((kept, dropped)) = keep_one_json_media_type(&mut body.content)
        {
            hits.push((format!("{} {}", method.to_uppercase(), path), kept, dropped));
        }
    });
    report.collapsed_requests.extend(hits);
}

/// Returns `Some((kept, dropped))` only when a genuinely different media
/// type was dropped (e.g. `application/xml`, `application/octet-stream`).
/// The *arr stack and most .NET-based APIs advertise the same JSON payload
/// under several labels (`application/json`, `text/json`,
/// `application/*+json`, `text/plain`); collapsing those is a no-op on the
/// wire, so we do it silently to keep build output readable. Anything we
/// can't recognize as a JSON-equivalent label gets surfaced.
fn keep_one_json_media_type(
    content: &mut indexmap::IndexMap<String, openapiv3::MediaType>,
) -> Option<(String, Vec<String>)> {
    if content.len() <= 1 {
        return None;
    }
    // Prefer the exact `application/json` media type — progenitor only
    // typecodes that one (or `application/json;…` parameterized variants).
    // Anything else (`text/json`, `application/*+json`) gets categorized as
    // Raw, which breaks the success-type unification this whole pass is
    // trying to achieve.
    let json_key = content
        .keys()
        .find(|k| *k == "application/json" || k.starts_with("application/json;"))
        .or_else(|| content.keys().find(|k| k.contains("json")))
        .cloned()?;
    let dropped: Vec<String> = content
        .keys()
        .filter(|k| **k != json_key)
        .cloned()
        .collect();
    content.retain(|k, _| *k == json_key);
    let surfaced: Vec<String> = dropped
        .into_iter()
        .filter(|k| !is_json_equivalent(k))
        .collect();
    (!surfaced.is_empty()).then_some((json_key, surfaced))
}

/// Progenitor panics (`response_types.len() <= 1`) when an operation lists
/// more than one success-range (2xx) response, because it can only emit a
/// single success type per generated fn. Real specs (e.g. Prowlarr) declare
/// both `200` and `201` for some create endpoints with different schemas.
///
/// We preserve every status code AND every response shape by replacing each
/// 2xx response's JSON schema with a synthetic `oneOf` union of all the
/// distinct shapes (including a `null` variant if any 2xx is empty-bodied).
/// Progenitor sees one unified success type across statuses → emits an
/// enum where each upstream response shape becomes a callable variant.
pub fn merge_success_response_schemas(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    let hits = merge_bucket(spec, is_progenitor_success);
    report.merged_success_responses.extend(hits);
}

/// Sibling of [`merge_success_response_schemas`] for progenitor's error
/// bucket (4xx/5xx + `default`). Progenitor runs the same
/// `response_types.len() <= 1` assertion across error responses, so
/// divergent shapes (e.g. `404` returns a body, `500` is empty) still
/// crash codegen without this pass.
pub fn merge_error_response_schemas(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    let hits = merge_bucket(spec, is_progenitor_error);
    report.merged_error_responses.extend(hits);
}

fn merge_bucket(
    spec: &mut OpenAPI,
    in_bucket: fn(&StatusCode) -> bool,
) -> Vec<(String, Vec<String>, usize)> {
    let mut hits: Vec<(String, Vec<String>, usize)> = Vec::new();
    for_each_op_mut(spec, |method, path, op| {
        let Some(op) = op else { return };
        // Both buckets include the `default` response (progenitor's
        // `is_success_or_default` / `is_error_or_default`). Unify schemas
        // across the whole bucket, otherwise progenitor's
        // `response_types.len() <= 1` assertion fires on divergent shapes.
        let statuses: Vec<SuccessKey> = op
            .responses
            .responses
            .keys()
            .filter(|s| in_bucket(s))
            .cloned()
            .map(SuccessKey::Status)
            .chain(op.responses.default.as_ref().map(|_| SuccessKey::Default))
            .collect();
        if statuses.len() <= 1 {
            return;
        }

        // Strip non-JSON content (e.g. a `503` carrying `text/html`) from
        // every bucketed response first. orca only ever deserializes the JSON
        // body, but progenitor counts each non-JSON content entry as its own
        // response *type* — so a bucket mixing a JSON schema and a `text/html`
        // body trips its `response_types.len() <= 1` assertion even after the
        // JSON schemas are unified below. Clearing the non-JSON content makes
        // such a response contribute the empty/`null` variant instead of a
        // distinct raw type. Only do this when the bucket has >1 status, so a
        // lone non-JSON response (its own success type) is left untouched.
        for key in &statuses {
            if let Some(resp) = get_success_response_mut(op, key)
                && !resp.content.is_empty()
                && !resp.content.keys().any(|k| k.contains("json"))
            {
                resp.content.clear();
            }
        }

        // Collect distinct response shapes by serde-value identity. A
        // missing JSON content entry contributes a synthetic `null`
        // variant so empty-body successes still round-trip.
        let mut variants: Vec<ReferenceOr<Schema>> = Vec::new();
        let mut had_empty = false;
        for key in &statuses {
            let Some(resp) = get_success_response(op, key) else {
                continue;
            };
            match json_schema(resp) {
                Some(schema) => push_distinct(&mut variants, schema),
                None => had_empty = true,
            }
        }
        if had_empty {
            push_distinct(&mut variants, ReferenceOr::Item(null_schema()));
        }
        if variants.len() <= 1 {
            return;
        }

        let union = ReferenceOr::Item(Schema {
            schema_data: SchemaData::default(),
            schema_kind: SchemaKind::OneOf {
                one_of: variants.clone(),
            },
        });
        for key in &statuses {
            let Some(resp) = get_success_response_mut(op, key) else {
                continue;
            };
            set_json_schema(resp, union.clone());
        }

        hits.push((
            format!("{} {}", method.to_uppercase(), path),
            statuses.iter().map(|k| k.label()).collect(),
            variants.len(),
        ));
    });
    hits
}

/// Mirror of `OperationResponseStatus::is_error_or_default` from
/// progenitor's method.rs (sans `Default`, which is tracked separately).
fn is_progenitor_error(s: &StatusCode) -> bool {
    match s {
        StatusCode::Code(c) => (400..600).contains(c),
        StatusCode::Range(4) | StatusCode::Range(5) => true,
        _ => false,
    }
}

#[derive(Clone, Debug)]
enum SuccessKey {
    Status(StatusCode),
    Default,
}

impl SuccessKey {
    fn label(&self) -> String {
        match self {
            SuccessKey::Status(s) => status_label(s),
            SuccessKey::Default => "default".into(),
        }
    }
}

fn get_success_response<'a>(
    op: &'a Operation,
    key: &SuccessKey,
) -> Option<&'a openapiv3::Response> {
    let r = match key {
        SuccessKey::Status(s) => op.responses.responses.get(s)?,
        SuccessKey::Default => op.responses.default.as_ref()?,
    };
    match r {
        ReferenceOr::Item(resp) => Some(resp),
        ReferenceOr::Reference { .. } => None,
    }
}

fn get_success_response_mut<'a>(
    op: &'a mut Operation,
    key: &SuccessKey,
) -> Option<&'a mut openapiv3::Response> {
    let r = match key {
        SuccessKey::Status(s) => op.responses.responses.get_mut(s)?,
        SuccessKey::Default => op.responses.default.as_mut()?,
    };
    match r {
        ReferenceOr::Item(resp) => Some(resp),
        ReferenceOr::Reference { .. } => None,
    }
}

/// Mirror of `OperationResponseStatus::is_success_or_default` from
/// progenitor's method.rs (sans `Default`, which is tracked separately).
fn is_progenitor_success(s: &StatusCode) -> bool {
    match s {
        StatusCode::Code(101) => true,
        StatusCode::Code(c) => (200..300).contains(c),
        StatusCode::Range(2) => true,
        StatusCode::Range(_) => false,
    }
}

fn status_label(s: &StatusCode) -> String {
    match s {
        StatusCode::Code(c) => c.to_string(),
        StatusCode::Range(r) => format!("{r}XX"),
    }
}

fn json_schema(resp: &openapiv3::Response) -> Option<ReferenceOr<Schema>> {
    resp.content
        .iter()
        .find(|(k, _)| k.contains("json"))
        .and_then(|(_, mt)| mt.schema.clone())
}

fn set_json_schema(resp: &mut openapiv3::Response, schema: ReferenceOr<Schema>) {
    // Ensure exactly one `application/json` entry, pointing at the union.
    let mt = resp.content.entry("application/json".into()).or_default();
    mt.schema = Some(schema);
}

fn push_distinct(out: &mut Vec<ReferenceOr<Schema>>, candidate: ReferenceOr<Schema>) {
    let cv = serde_json::to_value(&candidate).ok();
    if out
        .iter()
        .any(|existing| serde_json::to_value(existing).ok() == cv)
    {
        return;
    }
    out.push(candidate);
}

fn null_schema() -> Schema {
    Schema {
        schema_data: SchemaData {
            nullable: true,
            ..Default::default()
        },
        schema_kind: SchemaKind::Type(Type::String(StringType::default())),
    }
}

fn is_json_equivalent(media_type: &str) -> bool {
    let m = media_type.split(';').next().unwrap_or(media_type).trim();
    m.contains("json") || m == "text/plain"
}

fn synth_id(method: &str, path: &str) -> String {
    let mut s = String::from(method);
    for ch in path.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => s.push(ch),
            _ => s.push('_'),
        }
    }
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    s.trim_end_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openapiv3::{OpenAPI, ReferenceOr, StatusCode};

    fn spec(json: serde_json::Value) -> OpenAPI {
        serde_json::from_value(json).expect("valid openapi fixture")
    }

    fn base(paths: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "openapi": "3.0.0",
            "info": { "title": "t", "version": "0" },
            "paths": paths,
        })
    }

    #[test]
    fn synth_id_slugifies_and_collapses_underscores() {
        assert_eq!(synth_id("get", "/api/v3/movie/{id}"), "get_api_v3_movie_id");
        assert_eq!(synth_id("post", "/"), "post");
        assert_eq!(synth_id("get", "/a//b"), "get_a_b");
    }

    #[test]
    fn synthesize_assigns_ids_and_skips_existing() {
        let mut s = spec(base(serde_json::json!({
            "/a": { "get": { "responses": { "200": { "description": "ok" } } } },
            "/b": {
                "post": {
                    "operationId": "keepMe",
                    "responses": { "200": { "description": "ok" } }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        synthesize_operation_ids(&mut s, &mut r);
        assert_eq!(r.synthesized_ids.len(), 1);
        assert_eq!(r.synthesized_ids[0].2, "get_a");
        // Keep existing.
        let op_b = s.paths.paths.get("/b").unwrap();
        if let ReferenceOr::Item(item) = op_b {
            assert_eq!(
                item.post.as_ref().unwrap().operation_id.as_deref(),
                Some("keepMe")
            );
        } else {
            panic!("expected item");
        }
    }

    #[test]
    fn for_each_op_mut_iterates_all_methods() {
        let mut s = spec(base(serde_json::json!({
            "/p": {
                "get":     { "responses": { "200": { "description": "ok" } } },
                "put":     { "responses": { "200": { "description": "ok" } } },
                "post":    { "responses": { "200": { "description": "ok" } } },
                "delete":  { "responses": { "200": { "description": "ok" } } },
                "options": { "responses": { "200": { "description": "ok" } } },
                "head":    { "responses": { "200": { "description": "ok" } } },
                "patch":   { "responses": { "200": { "description": "ok" } } },
                "trace":   { "responses": { "200": { "description": "ok" } } }
            }
        })));
        let mut r = NormalizeReport::default();
        synthesize_operation_ids(&mut s, &mut r);
        assert_eq!(r.synthesized_ids.len(), 8);
        let mut methods: Vec<&str> = r
            .synthesized_ids
            .iter()
            .map(|(m, _, _)| m.as_str())
            .collect();
        methods.sort();
        assert_eq!(
            methods,
            vec![
                "delete", "get", "head", "options", "patch", "post", "put", "trace"
            ]
        );
    }

    #[test]
    fn rewrite_multipart_collapses_to_octet_stream() {
        let mut s = spec(base(serde_json::json!({
            "/login": {
                "post": {
                    "requestBody": {
                        "content": {
                            "multipart/form-data": { "schema": { "type": "object" } }
                        }
                    },
                    "responses": { "200": { "description": "ok" } }
                }
            },
            // No body — function must early-return.
            "/ping": { "get": { "responses": { "200": { "description": "ok" } } } },
            // Non-multipart body — untouched.
            "/json": {
                "post": {
                    "requestBody": {
                        "content": { "application/json": { "schema": { "type": "object" } } }
                    },
                    "responses": { "200": { "description": "ok" } }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        rewrite_multipart_to_octet_stream(&mut s, &mut r);
        assert_eq!(r.rewrote_multipart, vec!["POST /login".to_string()]);

        let login = s.paths.paths.get("/login").unwrap();
        let ReferenceOr::Item(item) = login else {
            panic!()
        };
        let body = item.post.as_ref().unwrap().request_body.as_ref().unwrap();
        let ReferenceOr::Item(body) = body else {
            panic!()
        };
        assert_eq!(
            body.content.keys().collect::<Vec<_>>(),
            vec!["application/octet-stream"]
        );
    }

    #[test]
    fn collapse_response_keeps_application_json_silently_when_others_are_equivalent() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": { "schema": { "type": "object" } },
                                "text/json":        { "schema": { "type": "object" } },
                                "text/plain":       { "schema": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        collapse_response_media_types(&mut s, &mut r);
        // All dropped types are json-equivalent → no entry surfaced.
        assert!(r.collapsed_responses.is_empty());

        let a = s.paths.paths.get("/a").unwrap();
        let ReferenceOr::Item(item) = a else { panic!() };
        let resp = item
            .get
            .as_ref()
            .unwrap()
            .responses
            .responses
            .get(&StatusCode::Code(200))
            .unwrap();
        let ReferenceOr::Item(resp) = resp else {
            panic!()
        };
        assert_eq!(
            resp.content.keys().collect::<Vec<_>>(),
            vec!["application/json"]
        );
    }

    #[test]
    fn collapse_response_surfaces_non_json_drops_and_handles_default() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": { "schema": { "type": "object" } },
                                "application/xml":  { "schema": { "type": "object" } }
                            }
                        },
                        "default": {
                            "description": "err",
                            "content": {
                                "application/json": { "schema": { "type": "object" } },
                                "application/octet-stream": { "schema": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        collapse_response_media_types(&mut s, &mut r);
        assert_eq!(r.collapsed_responses.len(), 2);
        let labels: Vec<&str> = r
            .collapsed_responses
            .iter()
            .map(|(l, _, _)| l.as_str())
            .collect();
        assert!(labels.iter().any(|l| l.contains("200")));
        assert!(labels.iter().any(|l| l.contains("default")));
    }

    #[test]
    fn collapse_response_when_no_application_json_falls_back_to_any_json_key() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/vnd.api+json": { "schema": { "type": "object" } },
                                "application/xml":          { "schema": { "type": "object" } }
                            }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        collapse_response_media_types(&mut s, &mut r);
        let a = s.paths.paths.get("/a").unwrap();
        let ReferenceOr::Item(item) = a else { panic!() };
        let resp = item
            .get
            .as_ref()
            .unwrap()
            .responses
            .responses
            .get(&StatusCode::Code(200))
            .unwrap();
        let ReferenceOr::Item(resp) = resp else {
            panic!()
        };
        assert_eq!(
            resp.content.keys().collect::<Vec<_>>(),
            vec!["application/vnd.api+json"]
        );
    }

    #[test]
    fn collapse_response_with_no_json_at_all_is_noop() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/xml":          { "schema": { "type": "object" } },
                                "application/octet-stream": { "schema": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        collapse_response_media_types(&mut s, &mut r);
        assert!(r.collapsed_responses.is_empty());
        let a = s.paths.paths.get("/a").unwrap();
        let ReferenceOr::Item(item) = a else { panic!() };
        let resp = item
            .get
            .as_ref()
            .unwrap()
            .responses
            .responses
            .get(&StatusCode::Code(200))
            .unwrap();
        let ReferenceOr::Item(resp) = resp else {
            panic!()
        };
        // Both kept — no JSON key to anchor on.
        assert_eq!(resp.content.len(), 2);
    }

    #[test]
    fn collapse_request_media_types_drops_xml_keeps_json() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "post": {
                    "requestBody": {
                        "content": {
                            "application/json": { "schema": { "type": "object" } },
                            "application/xml":  { "schema": { "type": "object" } }
                        }
                    },
                    "responses": { "200": { "description": "ok" } }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        collapse_request_media_types(&mut s, &mut r);
        assert_eq!(r.collapsed_requests.len(), 1);
        let a = s.paths.paths.get("/a").unwrap();
        let ReferenceOr::Item(item) = a else { panic!() };
        let body = item.post.as_ref().unwrap().request_body.as_ref().unwrap();
        let ReferenceOr::Item(body) = body else {
            panic!()
        };
        assert_eq!(
            body.content.keys().collect::<Vec<_>>(),
            vec!["application/json"]
        );
    }

    #[test]
    fn merge_success_unifies_divergent_2xx_into_oneof() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "post": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": { "schema": { "type": "object", "properties": { "id": { "type": "integer" } } } }
                            }
                        },
                        "201": {
                            "description": "created",
                            "content": {
                                "application/json": { "schema": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        merge_success_response_schemas(&mut s, &mut r);
        assert_eq!(r.merged_success_responses.len(), 1);
        let (_, statuses, variants) = &r.merged_success_responses[0];
        assert_eq!(*variants, 2);
        let mut s_sorted = statuses.clone();
        s_sorted.sort();
        assert_eq!(s_sorted, vec!["200", "201"]);

        // Both responses now point at the same oneOf schema.
        let a = s.paths.paths.get("/a").unwrap();
        let ReferenceOr::Item(item) = a else { panic!() };
        let responses = &item.post.as_ref().unwrap().responses;
        for code in [200, 201] {
            let resp = responses.responses.get(&StatusCode::Code(code)).unwrap();
            let ReferenceOr::Item(resp) = resp else {
                panic!()
            };
            let mt = resp.content.get("application/json").unwrap();
            let ReferenceOr::Item(sch) = mt.schema.as_ref().unwrap() else {
                panic!()
            };
            assert!(matches!(sch.schema_kind, SchemaKind::OneOf { .. }));
        }
    }

    #[test]
    fn merge_success_adds_null_variant_for_empty_body() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "post": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": { "schema": { "type": "object" } }
                            }
                        },
                        "204": { "description": "no content" }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        merge_success_response_schemas(&mut s, &mut r);
        assert_eq!(r.merged_success_responses.len(), 1);
        assert_eq!(r.merged_success_responses[0].2, 2);
    }

    #[test]
    fn merge_success_uses_default_response_as_part_of_bucket() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": { "schema": { "type": "object" } }
                            }
                        },
                        "default": {
                            "description": "fallback",
                            "content": {
                                "application/json": { "schema": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        // success bucket includes `default` per get_success_response logic
        merge_success_response_schemas(&mut s, &mut r);
        let (_, statuses, _) = &r.merged_success_responses[0];
        assert!(statuses.iter().any(|s| s == "default"));
    }

    #[test]
    fn merge_success_single_status_is_noop() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": { "application/json": { "schema": { "type": "object" } } }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        merge_success_response_schemas(&mut s, &mut r);
        assert!(r.merged_success_responses.is_empty());
    }

    #[test]
    fn merge_success_identical_schemas_emits_no_oneof() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": { "application/json": { "schema": { "type": "object" } } }
                        },
                        "201": {
                            "description": "ok",
                            "content": { "application/json": { "schema": { "type": "object" } } }
                        }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        merge_success_response_schemas(&mut s, &mut r);
        // Only one distinct shape across both statuses → no union needed.
        assert!(r.merged_success_responses.is_empty());
    }

    #[test]
    fn merge_error_unifies_4xx_5xx() {
        let mut s = spec(base(serde_json::json!({
            "/a": {
                "get": {
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": { "application/json": { "schema": { "type": "object" } } }
                        },
                        "404": {
                            "description": "missing",
                            "content": { "application/json": { "schema": { "type": "object" } } }
                        },
                        "500": { "description": "boom" }
                    }
                }
            }
        })));
        let mut r = NormalizeReport::default();
        merge_error_response_schemas(&mut s, &mut r);
        assert_eq!(r.merged_error_responses.len(), 1);
        let (_, statuses, variants) = &r.merged_error_responses[0];
        assert_eq!(*variants, 2);
        let mut sorted = statuses.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["404", "500"]);
    }

    #[test]
    fn is_progenitor_success_and_error_classifiers() {
        assert!(is_progenitor_success(&StatusCode::Code(101)));
        assert!(is_progenitor_success(&StatusCode::Code(200)));
        assert!(is_progenitor_success(&StatusCode::Code(299)));
        assert!(!is_progenitor_success(&StatusCode::Code(404)));
        assert!(is_progenitor_success(&StatusCode::Range(2)));
        assert!(!is_progenitor_success(&StatusCode::Range(4)));

        assert!(is_progenitor_error(&StatusCode::Code(404)));
        assert!(is_progenitor_error(&StatusCode::Code(500)));
        assert!(!is_progenitor_error(&StatusCode::Code(200)));
        assert!(is_progenitor_error(&StatusCode::Range(4)));
        assert!(is_progenitor_error(&StatusCode::Range(5)));
        assert!(!is_progenitor_error(&StatusCode::Range(2)));
    }

    #[test]
    fn status_label_formats_codes_and_ranges() {
        assert_eq!(status_label(&StatusCode::Code(200)), "200");
        assert_eq!(status_label(&StatusCode::Range(4)), "4XX");
        assert_eq!(SuccessKey::Default.label(), "default");
        assert_eq!(SuccessKey::Status(StatusCode::Code(201)).label(), "201");
    }

    #[test]
    fn is_json_equivalent_recognizes_common_variants() {
        assert!(is_json_equivalent("application/json"));
        assert!(is_json_equivalent("text/json"));
        assert!(is_json_equivalent("application/vnd.api+json"));
        assert!(is_json_equivalent("application/json; charset=utf-8"));
        assert!(is_json_equivalent("text/plain"));
        assert!(!is_json_equivalent("application/xml"));
        assert!(!is_json_equivalent("application/octet-stream"));
    }

    #[test]
    fn for_progenitor_runs_every_pass_and_is_idempotent() {
        let mut s = spec(base(serde_json::json!({
            "/login": {
                "post": {
                    "requestBody": {
                        "content": {
                            "multipart/form-data": { "schema": { "type": "object" } }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "ok",
                            "content": {
                                "application/json": { "schema": { "type": "object" } },
                                "application/xml":  { "schema": { "type": "object" } }
                            }
                        },
                        "404": { "description": "missing" },
                        "500": {
                            "description": "boom",
                            "content": { "application/json": { "schema": { "type": "string" } } }
                        }
                    }
                }
            }
        })));
        let r1 = for_progenitor(&mut s);
        assert!(!r1.synthesized_ids.is_empty());
        assert!(!r1.rewrote_multipart.is_empty());
        assert!(!r1.collapsed_responses.is_empty());
        assert!(!r1.merged_error_responses.is_empty());

        let r2 = for_progenitor(&mut s);
        // Idempotent: a second pass finds nothing else to do.
        assert!(r2.synthesized_ids.is_empty());
        assert!(r2.rewrote_multipart.is_empty());
        assert!(r2.collapsed_responses.is_empty());
        assert!(r2.collapsed_requests.is_empty());
        assert!(r2.merged_success_responses.is_empty());
        assert!(r2.merged_error_responses.is_empty());
    }

    #[test]
    fn emit_cargo_warnings_prints_all_buckets() {
        // Just exercise the println path — assert it doesn't panic and
        // covers every loop body.
        let r = NormalizeReport {
            synthesized_ids: vec![("get".into(), "/a".into(), "get_a".into())],
            rewrote_multipart: vec!["POST /login".into()],
            collapsed_requests: vec![(
                "POST /a".into(),
                "application/json".into(),
                vec!["application/xml".into()],
            )],
            collapsed_responses: vec![(
                "GET /a -> 200".into(),
                "application/json".into(),
                vec!["application/xml".into()],
            )],
            merged_success_responses: vec![("POST /a".into(), vec!["200".into(), "201".into()], 2)],
            merged_error_responses: vec![("GET /a".into(), vec!["404".into(), "500".into()], 2)],
            rewrote_deepobject_params: vec![("post /a".into(), "prefs".into())],
        };
        r.emit_cargo_warnings("test-crate");
    }

    #[test]
    fn deepobject_query_param_rewritten_to_form() {
        let mut s = spec(serde_json::json!({
            "openapi": "3.0.3",
            "info": { "title": "t", "version": "0" },
            "paths": {
                "/library/sections/all": {
                    "post": {
                        "operationId": "post_all",
                        "parameters": [{
                            "name": "prefs",
                            "in": "query",
                            "style": "deepObject",
                            "schema": { "type": "object" }
                        }],
                        "responses": { "200": { "description": "ok" } }
                    }
                }
            }
        }));
        let mut r = NormalizeReport::default();
        rewrite_deepobject_query_params(&mut s, &mut r);
        let ReferenceOr::Item(item) = &s.paths.paths["/library/sections/all"] else {
            panic!("expected item");
        };
        let ReferenceOr::Item(Parameter::Query { style, .. }) =
            &item.post.as_ref().unwrap().parameters[0]
        else {
            panic!("expected query param");
        };
        assert!(matches!(style, QueryStyle::Form), "deepObject -> form");
        assert_eq!(
            r.rewrote_deepobject_params,
            vec![(
                "post /library/sections/all".to_string(),
                "prefs".to_string()
            )]
        );
    }

    #[test]
    fn paths_with_reference_or_ref_are_skipped() {
        // path item is a `$ref` rather than an inline PathItem — iterator
        // hits the `continue` branch.
        let s_json = serde_json::json!({
            "openapi": "3.0.0",
            "info": { "title": "t", "version": "0" },
            "paths": {
                "/a": { "$ref": "#/components/pathItems/foo" }
            }
        });
        let mut s: OpenAPI = serde_json::from_value(s_json).unwrap();
        let mut r = NormalizeReport::default();
        synthesize_operation_ids(&mut s, &mut r);
        assert!(r.synthesized_ids.is_empty());
    }

    #[test]
    fn response_reference_is_treated_as_no_schema() {
        // ReferenceOr::Reference in the success bucket → get_success_response
        // returns None, which contributes nothing to the variants list.
        let s_json = serde_json::json!({
            "openapi": "3.0.0",
            "info": { "title": "t", "version": "0" },
            "components": {
                "responses": {
                    "Shared": { "description": "shared" }
                }
            },
            "paths": {
                "/a": {
                    "get": {
                        "responses": {
                            "200": { "$ref": "#/components/responses/Shared" },
                            "201": {
                                "description": "ok",
                                "content": { "application/json": { "schema": { "type": "object" } } }
                            }
                        }
                    }
                }
            }
        });
        let mut s: OpenAPI = serde_json::from_value(s_json).unwrap();
        let mut r = NormalizeReport::default();
        merge_success_response_schemas(&mut s, &mut r);
        // Only one inline shape — no union emitted.
        assert!(r.merged_success_responses.is_empty());
    }
}
