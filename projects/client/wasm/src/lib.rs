//! wasm-bindgen wrapper around `orca_client_core`.
//!
//! Exposes an `OrcaClient` JS class to the frontend. All business logic
//! (request shaping, decoding, retries) lives in `orca_client_core`; this
//! crate only contributes the browser-fetch transport and JS marshalling.

use async_trait::async_trait;
use orca_client_core::{
    ClientError, HttpRequest, HttpResponse, OrcaClient as CoreClient, Transport,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

struct FetchTransport;

#[async_trait(?Send)]
impl Transport for FetchTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, ClientError> {
        let opts = web_sys::RequestInit::new();
        opts.set_method(req.method);
        opts.set_mode(web_sys::RequestMode::Cors);

        let request = web_sys::Request::new_with_str_and_init(&req.url, &opts)
            .map_err(|e| ClientError::Transport(jsval_to_string(&e)))?;

        let window =
            web_sys::window().ok_or_else(|| ClientError::Transport("no global window".into()))?;

        let resp_value = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| ClientError::Transport(jsval_to_string(&e)))?;

        let resp: web_sys::Response = resp_value
            .dyn_into()
            .map_err(|_| ClientError::Transport("response not a Response".into()))?;
        let status = resp.status();

        let buf_promise = resp
            .array_buffer()
            .map_err(|e| ClientError::Transport(jsval_to_string(&e)))?;
        let buf_value = JsFuture::from(buf_promise)
            .await
            .map_err(|e| ClientError::Transport(jsval_to_string(&e)))?;
        let bytes = js_sys::Uint8Array::new(&buf_value).to_vec();

        Ok(HttpResponse {
            status,
            body: bytes,
        })
    }
}

fn jsval_to_string(v: &JsValue) -> String {
    v.as_string()
        .or_else(|| js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "<unknown>".into())
}

fn err_to_js(e: ClientError) -> JsValue {
    JsValue::from_str(&e.to_string())
}

#[wasm_bindgen]
pub struct OrcaClient {
    inner: CoreClient<FetchTransport>,
}

#[wasm_bindgen]
impl OrcaClient {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: String) -> OrcaClient {
        OrcaClient {
            inner: CoreClient::new(base_url, FetchTransport),
        }
    }

    pub async fn health(&self) -> Result<JsValue, JsValue> {
        let h = self.inner.health().await.map_err(err_to_js)?;
        serde_wasm_bindgen::to_value(&serde_json::json!({ "ok": h.ok }))
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}
