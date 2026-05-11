//! WASM client surface — only compiled with `feature = "wasm"`.
//!
//! Exposes an `OrcaClient` JS class whose methods are emitted by the
//! `declare_tools!` macro in `lib.rs`. Each method POSTs to
//! `<base_url>/api/tools/<NAME>` with the args JSON-serialized as the body
//! and returns the parsed response as a `JsValue` (objects, not Maps).

use serde::{Serialize, de::DeserializeOwned};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::OrcaToolDef;

#[wasm_bindgen]
pub struct OrcaClient {
    base_url: String,
}

#[wasm_bindgen]
impl OrcaClient {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: String) -> OrcaClient {
        OrcaClient { base_url }
    }
}

impl OrcaClient {
    /// Typed dispatch path used by every macro-emitted per-tool method.
    /// Serializes `Args` → JSON, POSTs to `/api/tools/<NAME>`, deserializes
    /// the response into `Output`.
    pub async fn call_tool_typed<T: OrcaToolDef>(&self, args: T::Args) -> Result<T::Output, JsValue>
    where
        T::Args: Serialize,
        T::Output: DeserializeOwned,
    {
        let body_str = serde_json::to_string(&args)
            .map_err(|e| JsValue::from_str(&format!("serialize args: {e}")))?;
        let value = self.call_raw(T::NAME, &body_str).await?;
        serde_wasm_bindgen::from_value(value)
            .map_err(|e| JsValue::from_str(&format!("decode output: {e}")))
    }

    /// Internal — invoked by every macro-emitted per-tool method.
    pub async fn call_tool(&self, name: &str, args: JsValue) -> Result<JsValue, JsValue> {
        // Stringify the incoming args. JS callers pass plain objects; we
        // want a stable JSON body the server can deserialize.
        let body_str = js_sys::JSON::stringify(&args)
            .map_err(|e| jsval_to_err("stringify args", &e))?
            .as_string()
            .unwrap_or_else(|| "{}".to_string());
        self.call_raw(name, &body_str).await
    }

    /// Lowest-level HTTP call — POST a pre-serialized JSON body to
    /// `/api/tools/<name>` and return the parsed response.
    async fn call_raw(&self, name: &str, body_str: &str) -> Result<JsValue, JsValue> {
        let url = format!("{}/api/tools/{}", self.base_url, name);

        let opts = web_sys::RequestInit::new();
        opts.set_method("POST");
        opts.set_mode(web_sys::RequestMode::Cors);
        opts.set_body(&JsValue::from_str(body_str));

        let headers = web_sys::Headers::new().map_err(|e| jsval_to_err("headers", &e))?;
        headers
            .set("content-type", "application/json")
            .map_err(|e| jsval_to_err("set header", &e))?;
        opts.set_headers(&headers);

        let request = web_sys::Request::new_with_str_and_init(&url, &opts)
            .map_err(|e| jsval_to_err("build request", &e))?;

        let window = web_sys::window().ok_or_else(|| JsValue::from_str("no global window"))?;
        let resp_value = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| jsval_to_err("fetch", &e))?;

        let resp: web_sys::Response = resp_value
            .dyn_into()
            .map_err(|_| JsValue::from_str("response not a Response"))?;

        // Read JSON body via Response.json() — returns a JsValue (object form).
        let json_promise = resp.json().map_err(|e| jsval_to_err("json()", &e))?;
        let value = JsFuture::from(json_promise)
            .await
            .map_err(|e| jsval_to_err("parse json", &e))?;

        if !resp.ok() {
            let status = resp.status();
            return Err(JsValue::from_str(&format!(
                "HTTP {status}: {}",
                js_sys::JSON::stringify(&value)
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or_default()
            )));
        }

        Ok(value)
    }
}

fn jsval_to_err(label: &str, v: &JsValue) -> JsValue {
    let detail = v
        .as_string()
        .or_else(|| js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "<unknown>".into());
    JsValue::from_str(&format!("{label}: {detail}"))
}

/// Helper used by macro-emitted methods to serialize the args struct passed
/// from JS into a `JsValue` suitable for `call_tool`. Uses the
/// object-emitting serializer (not the default Map one).
pub fn serialize_args<T: Serialize>(args: &T) -> Result<JsValue, JsValue> {
    let ser = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    args.serialize(&ser)
        .map_err(|e| JsValue::from_str(&e.to_string()))
}
