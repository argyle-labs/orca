use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderValue, header::HeaderName},
    middleware::Next,
    response::Response,
};
use http_body_util::BodyExt;
use uuid::Uuid;

pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";

#[derive(Clone)]
pub struct CorrelationId(pub String);

pub async fn log_requests(req: Request, next: Next) -> Response {
    let cid = req
        .headers()
        .get(CORRELATION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let at_trace = tracing::enabled!(tracing::Level::TRACE);

    let mut req = if at_trace {
        let (parts, body) = req.into_parts();
        let bytes = collect_body(body).await;
        tracing::trace!(
            correlation_id = %cid,
            method = %method,
            path = %uri,
            body = %lossy_truncate(&bytes),
            "→ request"
        );
        Request::from_parts(parts, Body::from(bytes))
    } else {
        tracing::info!(correlation_id = %cid, method = %method, path = %uri, "→ request");
        req
    };

    req.extensions_mut().insert(CorrelationId(cid.clone()));

    let response = next.run(req).await;
    let status = response.status().as_u16();

    let (mut parts, body) = response.into_parts();
    if let Ok(val) = HeaderValue::from_str(&cid) {
        parts
            .headers
            .insert(HeaderName::from_static(CORRELATION_ID_HEADER), val);
    }

    if at_trace {
        let bytes = collect_body(body).await;
        tracing::trace!(
            correlation_id = %cid,
            status = %status,
            body = %lossy_truncate(&bytes),
            "← response"
        );
        Response::from_parts(parts, Body::from(bytes))
    } else {
        tracing::info!(correlation_id = %cid, status = %status, "← response");
        Response::from_parts(parts, body)
    }
}

async fn collect_body(body: Body) -> Bytes {
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default()
}

fn lossy_truncate(bytes: &Bytes) -> String {
    const MAX: usize = 2048;
    let s = String::from_utf8_lossy(bytes);
    if s.len() > MAX {
        format!("{}…[{} bytes total]", &s[..MAX], bytes.len())
    } else {
        s.into_owned()
    }
}
