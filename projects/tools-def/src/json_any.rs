//! `JsonAny` — opaque JSON wrapper used as Output for tools that return
//! shape-fluid upstream payloads (e.g. Home Assistant entity dumps, Proxmox
//! cluster listings). Serializes transparently as the inner value; TS sees it
//! as `unknown` rather than a useless stringified blob.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opaque JSON passthrough wrapper for genuinely free-form upstream payloads
/// (e.g. Home Assistant entity dumps, Proxmox cluster listings, MCP structuredContent).
/// Using `Value` here is intentional — the upstream schema is not owned by orca.
#[allow(clippy::disallowed_types)]
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct JsonAny(#[cfg_attr(feature = "wasm", tsify(type = "unknown"))] pub Value);

#[allow(clippy::disallowed_types)]
impl From<Value> for JsonAny {
    fn from(v: Value) -> Self {
        Self(v)
    }
}
