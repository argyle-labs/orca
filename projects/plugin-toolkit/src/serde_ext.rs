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

/// Coerce a JSON value that *means* a number into `f64`. Accepts a real JSON
/// number and a numeric string (`"0.00"`, `"42"`). Proxmox VE documents its PSI
/// `pressure*` fields as `number` but serializes them as quoted strings — the
/// declared type stays `f64` and the reconciliation lands here, at the seam.
/// Returns `None` for anything that isn't number-ish (including JSON null).
fn coerce_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Deserialize an `f64` that may arrive as a JSON number or a numeric string.
/// Errors only when the value can't be read as a number at all.
pub fn f64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    coerce_number(&v)
        .ok_or_else(|| D::Error::custom(format!("expected a number-ish value, got {v}")))
}

/// [`f64_lenient`] for `Option<f64>` fields. JSON `null` (and, with
/// `#[serde(default)]`, an absent key) becomes `None`; any present value is
/// coerced. Pair with `#[serde(default)]` so a missing key stays `None`.
pub fn opt_f64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    if v.is_null() {
        return Ok(None);
    }
    coerce_number(&v)
        .map(Some)
        .ok_or_else(|| D::Error::custom(format!("expected a number-ish value, got {v}")))
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

    #[derive(Deserialize)]
    struct HasNum {
        #[serde(deserialize_with = "f64_lenient")]
        n: f64,
    }
    #[derive(Deserialize)]
    struct HasOptNum {
        #[serde(default, deserialize_with = "opt_f64_lenient")]
        n: Option<f64>,
    }

    #[test]
    fn accepts_number_and_numeric_string() {
        // The Proxmox PSI case: documented number, wire sends "0.00".
        assert_eq!(
            serde_json::from_str::<HasNum>(r#"{"n":"0.00"}"#).unwrap().n,
            0.0
        );
        assert_eq!(
            serde_json::from_str::<HasNum>(r#"{"n":3.5}"#).unwrap().n,
            3.5
        );
        assert_eq!(
            serde_json::from_str::<HasNum>(r#"{"n":"42"}"#).unwrap().n,
            42.0
        );
    }

    #[test]
    fn rejects_non_number() {
        assert!(serde_json::from_str::<HasNum>(r#"{"n":"nope"}"#).is_err());
        assert!(serde_json::from_str::<HasNum>(r#"{"n":[]}"#).is_err());
    }

    #[test]
    fn opt_number_handles_null_absent_and_string() {
        assert_eq!(
            serde_json::from_str::<HasOptNum>(r#"{"n":null}"#)
                .unwrap()
                .n,
            None
        );
        assert_eq!(serde_json::from_str::<HasOptNum>(r#"{}"#).unwrap().n, None);
        assert_eq!(
            serde_json::from_str::<HasOptNum>(r#"{"n":"1.5"}"#)
                .unwrap()
                .n,
            Some(1.5)
        );
    }
}
