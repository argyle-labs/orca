//! Reusable preprocessors that bring upstream OpenAPI specs in line with
//! what `progenitor` can codegen. Real-world specs (Sonarr, Radarr, Jellyfin,
//! …) routinely miss `operationId`, mix multipart bodies, or list multiple
//! response media types per status — all of which progenitor rejects.
//!
//! Each fix lives here so a new consumer crate only writes a thin build.rs:
//!
//! ```ignore
//! let mut spec: openapiv3::OpenAPI = serde_json::from_str(&raw)?;
//! integrations_openapi::normalize::for_progenitor(&mut spec);
//! let tokens = progenitor::Generator::default().generate_tokens(&spec)?;
//! ```
//!
//! Add new preprocessors here as we discover more upstream-spec edge cases;
//! never patch them in a single integration's build.rs.

use openapiv3::{OpenAPI, Operation, ReferenceOr};

/// Run the full preprocessor chain that maps "imperfect but valid OpenAPI"
/// to "what progenitor accepts." Idempotent.
pub fn for_progenitor(spec: &mut OpenAPI) {
    synthesize_operation_ids(spec);
    strip_multipart_operations(spec);
    collapse_response_media_types(spec);
    collapse_request_media_types(spec);
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
pub fn synthesize_operation_ids(spec: &mut OpenAPI) {
    for_each_op_mut(spec, |method, path, op| {
        if let Some(op) = op
            && op.operation_id.is_none()
        {
            op.operation_id = Some(synth_id(method, path));
        }
    });
}

/// Drop any operation that uses a `multipart/*` request body. Progenitor
/// doesn't support multipart codegen; callers that need file-upload
/// endpoints (e.g. Sonarr's manual-import) fall back to raw reqwest.
pub fn strip_multipart_operations(spec: &mut OpenAPI) {
    for_each_op_mut(spec, |_method, _path, op| {
        if let Some(o) = op.as_ref()
            && let Some(ReferenceOr::Item(body)) = &o.request_body
            && body.content.keys().any(|k| k.starts_with("multipart/"))
        {
            *op = None;
        }
    });
}

/// Progenitor errors with "more media types than expected" when a response
/// (or request) lists more than one media type. Real specs commonly serve
/// the same payload as `application/json` + `text/json` + `application/*+json`
/// — orca only ever wants the JSON one. Keep the first JSON-ish entry and
/// drop the rest.
pub fn collapse_response_media_types(spec: &mut OpenAPI) {
    for_each_op_mut(spec, |_m, _p, op| {
        let Some(op) = op else { return };
        for resp in op.responses.responses.values_mut() {
            if let ReferenceOr::Item(r) = resp {
                keep_one_json_media_type(&mut r.content);
            }
        }
        if let Some(ReferenceOr::Item(r)) = op.responses.default.as_mut() {
            keep_one_json_media_type(&mut r.content);
        }
    });
}

/// Same idea as `collapse_response_media_types`, applied to request bodies.
pub fn collapse_request_media_types(spec: &mut OpenAPI) {
    for_each_op_mut(spec, |_m, _p, op| {
        if let Some(op) = op
            && let Some(ReferenceOr::Item(body)) = op.request_body.as_mut()
        {
            keep_one_json_media_type(&mut body.content);
        }
    });
}

fn keep_one_json_media_type(content: &mut indexmap::IndexMap<String, openapiv3::MediaType>) {
    if content.len() <= 1 {
        return;
    }
    let json_key = content
        .keys()
        .find(|k| k.contains("json"))
        .cloned()
        .or_else(|| content.keys().next().cloned());
    let Some(json_key) = json_key else { return };
    content.retain(|k, _| *k == json_key);
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
