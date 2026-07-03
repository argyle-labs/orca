//! Lenient deserializers for wire representations that don't match a field's
//! declared schema type.
//!
//! Some APIs document a field as one JSON type but serialize it as another.
//! Proxmox VE is the canonical case: its API docs declare `template`,
//! `running`, and friends as `boolean`, yet the wire body carries integer
//! `0`/`1`. "Inline with the docs" means the generated type stays `bool` — the
//! reconciliation belongs here, at the deserialize seam, not in a hand-patched
//! type or a per-call-site `as` cast.
//!
//! The OpenAPI codegen ([`plugin_toolkit_build::openapi`]) opts a plugin into
//! anchoring these on every `bool` / `Option<bool>` field, so no plugin writes
//! the `#[serde(deserialize_with = …)]` itself.

// This module's whole job is coercing arbitrary JSON into a bool, so it reads
// the untyped `serde_json::Value` deliberately — the same stance as the openapi
// codegen's raw-spec handling.
#![allow(clippy::disallowed_types)]

use serde::{Deserialize, Deserializer, de::Error as _};

/// Coerce a JSON value that *means* a boolean into one. Accepts a real boolean,
/// integer `0`/`1`, and the strings `"0"/"1"/"true"/"false"/"yes"/"no"/"on"/"off"`
/// (case-insensitive). Returns `None` for anything else (including JSON null).
fn coerce(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => n.as_i64().map(|i| i != 0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Deserialize a `bool` that may arrive as a boolean, integer `0`/`1`, or a
/// boolean-ish string. Errors only when the value can't be read as a boolean at
/// all — a stricter contract than "accept anything truthy".
pub fn bool_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    coerce(&v).ok_or_else(|| D::Error::custom(format!("expected a boolean-ish value, got {v}")))
}

/// [`bool_lenient`] for `Option<bool>` fields. JSON `null` (and, with
/// `#[serde(default)]`, an absent key) becomes `None`; any present value is
/// coerced. Pair with `#[serde(default)]` so a missing key stays `None`.
pub fn opt_bool_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    coerce(&v)
        .map(Some)
        .ok_or_else(|| D::Error::custom(format!("expected a boolean-ish value, got {v}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct HasBool {
        #[serde(deserialize_with = "bool_lenient")]
        flag: bool,
    }

    #[derive(Deserialize)]
    struct HasOptBool {
        #[serde(default, deserialize_with = "opt_bool_lenient")]
        flag: Option<bool>,
    }

    fn flag(json: &str) -> bool {
        serde_json::from_str::<HasBool>(json).unwrap().flag
    }
    fn opt(json: &str) -> Option<bool> {
        serde_json::from_str::<HasOptBool>(json).unwrap().flag
    }

    #[test]
    fn accepts_integer_booleans() {
        assert!(flag(r#"{"flag":1}"#));
        assert!(!flag(r#"{"flag":0}"#));
    }

    #[test]
    fn accepts_real_and_string_booleans() {
        assert!(flag(r#"{"flag":true}"#));
        assert!(!flag(r#"{"flag":false}"#));
        assert!(flag(r#"{"flag":"yes"}"#));
        assert!(!flag(r#"{"flag":"off"}"#));
    }

    #[test]
    fn rejects_non_boolean() {
        assert!(serde_json::from_str::<HasBool>(r#"{"flag":"maybe"}"#).is_err());
        assert!(serde_json::from_str::<HasBool>(r#"{"flag":[]}"#).is_err());
    }

    #[test]
    fn option_handles_null_absent_and_int() {
        assert_eq!(opt(r#"{"flag":null}"#), None);
        assert_eq!(opt(r#"{}"#), None);
        assert_eq!(opt(r#"{"flag":1}"#), Some(true));
        assert_eq!(opt(r#"{"flag":0}"#), Some(false));
    }
}
