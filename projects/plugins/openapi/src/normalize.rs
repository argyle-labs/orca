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
    MediaType, OpenAPI, Operation, ReferenceOr, Schema, SchemaData, SchemaKind, StatusCode,
    StringFormat, StringType, Type, VariantOrUnknownOrEmpty,
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
    r
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
