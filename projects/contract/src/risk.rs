//! How dangerous a verb is, declared per tool via `#[orca_tool(risk = "...")]`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Ordered `Low < Medium < High`. Only `High` is ever declared explicitly;
/// derivation never produces it.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    #[default]
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Risk::Low),
            "medium" => Some(Risk::Medium),
            "high" => Some(Risk::High),
            _ => None,
        }
    }

    /// Risk for a verb that declared none: reads are low, everything else is
    /// medium. Never `High`.
    pub fn derived(read_shaped: bool) -> Self {
        if read_shaped { Risk::Low } else { Risk::Medium }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_low_to_high() {
        assert!(Risk::Low < Risk::Medium && Risk::Medium < Risk::High);
        assert_eq!(Risk::Low.max(Risk::High), Risk::High);
    }

    #[test]
    fn serde_is_lowercase_and_round_trips() {
        for r in [Risk::Low, Risk::Medium, Risk::High] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(s, format!("\"{}\"", r.as_str()));
            assert_eq!(serde_json::from_str::<Risk>(&s).unwrap(), r);
            assert_eq!(Risk::parse(r.as_str()), Some(r));
        }
        assert!(serde_json::from_str::<Risk>("\"High\"").is_err());
        assert_eq!(Risk::parse("critical"), None);
    }

    #[test]
    fn derivation_never_yields_high() {
        assert_eq!(Risk::derived(true), Risk::Low);
        assert_eq!(Risk::derived(false), Risk::Medium);
    }
}
