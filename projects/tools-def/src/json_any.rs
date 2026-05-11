//! `JsonAny` — opaque JSON wrapper used as Output for tools that return
//! shape-fluid upstream payloads (e.g. Home Assistant entity dumps, Proxmox
//! cluster listings). Serializes transparently as the inner value; TS sees it
//! as `unknown` rather than a useless stringified blob.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct JsonAny(#[cfg_attr(feature = "wasm", tsify(type = "unknown"))] pub Value);

impl From<Value> for JsonAny {
    fn from(v: Value) -> Self {
        Self(v)
    }
}
