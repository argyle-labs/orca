//! YAML deserialization — the one place in the workspace that knows how orca
//! parses YAML (OpenAPI specs, GraphQL descriptors). **Every callsite that used
//! to inline `serde_yaml::…` should call through here.** The backing library is
//! hidden: no caller names it. This is an abstraction, not a re-export.
//!
//! Motivation beyond consistency: `serde_yaml` is unmaintained upstream, so
//! confining it behind this seam means it can be swapped (e.g. `serde_yml`,
//! `saphyr`) with zero churn at any call site. Gated by the `yaml` feature so a
//! consumer that never parses YAML links no YAML parser.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

/// Deserialize a YAML document into `T`. Errors carry a parse-failure context.
pub fn from_str<T: DeserializeOwned>(raw: &str) -> Result<T> {
    serde_yaml::from_str(raw).context("parse YAML")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_into_typed_value() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Doc {
            name: String,
            count: u32,
        }
        let doc: Doc = from_str("name: orca\ncount: 3\n").unwrap();
        assert_eq!(
            doc,
            Doc {
                name: "orca".into(),
                count: 3
            }
        );
    }

    #[test]
    fn parses_sequences() {
        #[derive(serde::Deserialize)]
        struct Doc {
            a: Vec<u32>,
        }
        let doc: Doc = from_str("a: [1, 2]\n").unwrap();
        assert_eq!(doc.a, vec![1, 2]);
    }

    #[test]
    fn invalid_yaml_errors() {
        #[derive(serde::Deserialize)]
        struct Doc {
            #[allow(dead_code)]
            a: Vec<u32>,
        }
        let r: Result<Doc> = from_str("a: [unterminated");
        assert!(r.is_err());
    }
}
