//! `JsonAny` — opaque JSON wrapper used as Output for tools that return
//! shape-fluid upstream payloads (e.g. Home Assistant entity dumps, Proxmox
//! cluster listings). Serializes transparently as the inner value; TS sees it
//! as `unknown` rather than a useless stringified blob.
// JsonAny IS the designated opaque escape hatch — all Value/JsonAny uses in
// this file are intentional by definition.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opaque JSON passthrough wrapper for genuinely free-form upstream payloads
/// (e.g. Home Assistant entity dumps, Proxmox cluster listings, MCP structuredContent).
/// Using `Value` here is intentional — the upstream schema is not owned by orca.
#[allow(clippy::disallowed_types)]
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct JsonAny(pub Value);

#[allow(clippy::disallowed_types)]
impl From<Value> for JsonAny {
    fn from(v: Value) -> Self {
        Self(v)
    }
}

impl Default for JsonAny {
    fn default() -> Self {
        Self(Value::Null)
    }
}

/// Parse a JSON literal — used by clap so CLI flags like
/// `--args '{"name":"foo"}'` deserialize into a `JsonAny`.
impl std::str::FromStr for JsonAny {
    type Err = serde_json::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(serde_json::from_str(s)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    #[test]
    fn default_is_null() {
        let v: JsonAny = Default::default();
        assert_eq!(v.0, Value::Null);
    }

    #[test]
    fn from_value_wraps_inner() {
        let v: JsonAny = json!({"name": "foo"}).into();
        assert_eq!(v.0, json!({"name": "foo"}));
    }

    #[test]
    fn from_str_parses_json_literal() {
        let v = JsonAny::from_str(r#"{"k":1}"#).unwrap();
        assert_eq!(v.0, json!({"k": 1}));
    }

    #[test]
    fn from_str_rejects_invalid_json() {
        assert!(JsonAny::from_str("not json").is_err());
    }

    #[test]
    fn serializes_transparently_as_inner() {
        let v: JsonAny = json!([1, 2, 3]).into();
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, "[1,2,3]");
        let round: JsonAny = serde_json::from_str(&s).unwrap();
        assert_eq!(round.0, json!([1, 2, 3]));
    }

    #[test]
    fn clone_preserves_inner() {
        let v: JsonAny = json!("hello").into();
        assert_eq!(v.clone().0, v.0);
    }
}
