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

/// What `for_progenitor` had to change. Surfaced so consumer build scripts
/// can `cargo:warning=` each entry — that way a new upstream spec version
/// adding a multipart endpoint (or any other normalization hit) shows up in
/// the build log instead of silently disappearing from the generated client.
#[derive(Debug, Default, Clone)]
pub struct NormalizeReport {
    /// Synthesized operationIds: `(method, path, generated_id)`.
    pub synthesized_ids: Vec<(String, String, String)>,
    /// Operations dropped because they used a multipart request body.
    pub dropped_multipart: Vec<String>,
    /// Request bodies whose alternate media types were collapsed away.
    /// `(op_label, kept, dropped)`.
    pub collapsed_requests: Vec<(String, String, Vec<String>)>,
    /// Responses whose alternate media types were collapsed away.
    /// `(op_label + status, kept, dropped)`.
    pub collapsed_responses: Vec<(String, String, Vec<String>)>,
}

impl NormalizeReport {
    /// Emit `cargo:warning=` lines so each item appears in the build log.
    /// Intended for use from a consumer's build.rs.
    pub fn emit_cargo_warnings(&self, crate_name: &str) {
        for op in &self.dropped_multipart {
            println!("cargo:warning={crate_name}: dropped multipart op {op}");
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
    }
}

/// Run the full preprocessor chain that maps "imperfect but valid OpenAPI"
/// to "what progenitor accepts." Idempotent. Returns a report of every
/// change made — `()`-discard if you don't care.
pub fn for_progenitor(spec: &mut OpenAPI) -> NormalizeReport {
    let mut r = NormalizeReport::default();
    synthesize_operation_ids(spec, &mut r);
    strip_multipart_operations(spec, &mut r);
    collapse_response_media_types(spec, &mut r);
    collapse_request_media_types(spec, &mut r);
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

/// Drop any operation that uses a `multipart/*` request body. Progenitor
/// doesn't support multipart codegen; callers that need file-upload
/// endpoints (e.g. Sonarr's manual-import) fall back to raw reqwest.
pub fn strip_multipart_operations(spec: &mut OpenAPI, report: &mut NormalizeReport) {
    for_each_op_mut(spec, |method, path, op| {
        if let Some(o) = op.as_ref()
            && let Some(ReferenceOr::Item(body)) = &o.request_body
            && body.content.keys().any(|k| k.starts_with("multipart/"))
        {
            report
                .dropped_multipart
                .push(format!("{} {}", method.to_uppercase(), path));
            *op = None;
        }
    });
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
        let statuses: Vec<_> = op
            .responses
            .responses
            .keys()
            .map(|s| format!("{s:?}"))
            .collect();
        for (status_str, resp) in op
            .responses
            .responses
            .values_mut()
            .zip(statuses.iter())
            .map(|(r, s)| (s, r))
        {
            if let ReferenceOr::Item(r) = resp
                && let Some((kept, dropped)) = keep_one_json_media_type(&mut r.content)
            {
                hits.push((format!("{label} -> {status_str}"), kept, dropped));
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

/// Returns `Some((kept, dropped))` if anything was removed.
fn keep_one_json_media_type(
    content: &mut indexmap::IndexMap<String, openapiv3::MediaType>,
) -> Option<(String, Vec<String>)> {
    if content.len() <= 1 {
        return None;
    }
    let json_key = content.keys().find(|k| k.contains("json")).cloned()?;
    let dropped: Vec<String> = content
        .keys()
        .filter(|k| **k != json_key)
        .cloned()
        .collect();
    content.retain(|k, _| *k == json_key);
    Some((json_key, dropped))
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
