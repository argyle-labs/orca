//! Wall-clock time utility.
//!
//! `chrono` is an internal detail of this module: every other crate reaches UTC
//! wall-clock time through [`Timestamp`] (or the free helpers) and never names
//! the datetime library, so it can be swapped without touching callers. Per
//! [[orca-north-star-abstract-system-differences]].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Seconds since the Unix epoch as i64, saturating to 0 on the
/// (effectively impossible) pre-epoch clock case. Centralizes the
/// `SystemTime::now().duration_since(UNIX_EPOCH)...` boilerplate.
pub fn now_secs_since_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current time as an RFC3339 string. Single source of truth so
/// mesh/replication timestamps stay byte-identical across crates.
pub fn now_rfc3339() -> String {
    Timestamp::now().to_rfc3339()
}

/// The current UTC instant.
pub fn now() -> Timestamp {
    Timestamp::now()
}

/// A UTC instant. orca-owned; hides `chrono::DateTime<Utc>`. Ordered, copyable,
/// and (de)serialized as an RFC 3339 string (wire-compatible with a bare
/// `DateTime<Utc>`, so a field can migrate from one to the other transparently).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    /// The current UTC instant.
    pub fn now() -> Self {
        Self(Utc::now())
    }

    /// RFC 3339 / ISO 8601 with second precision and a `Z` suffix
    /// (`2026-07-09T18:20:05Z`).
    pub fn to_rfc3339(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Parse an RFC 3339 string into a UTC timestamp.
    pub fn parse_rfc3339(s: &str) -> Result<Self, ParseError> {
        DateTime::parse_from_rfc3339(s)
            .map(|dt| Self(dt.with_timezone(&Utc)))
            .map_err(|e| ParseError(e.to_string()))
    }

    /// Seconds since the Unix epoch.
    pub fn unix_seconds(&self) -> i64 {
        self.0.timestamp()
    }

    /// Milliseconds since the Unix epoch.
    pub fn unix_millis(&self) -> i64 {
        self.0.timestamp_millis()
    }

    /// A timestamp `secs` seconds after the Unix epoch, or `None` if out of range.
    pub fn from_unix_seconds(secs: i64) -> Option<Self> {
        DateTime::from_timestamp(secs, 0).map(Self)
    }

    /// A sortable compact stamp (`YYYYMMDD-HHMMSS`) for naming artifacts.
    pub fn compact(&self) -> String {
        self.0.format("%Y%m%d-%H%M%S").to_string()
    }

    /// This instant plus `dur`. Saturates on overflow.
    pub fn plus(&self, dur: Duration) -> Self {
        Self(self.0 + chrono::Duration::from_std(dur).unwrap_or_else(|_| chrono::Duration::zero()))
    }

    /// This instant minus `dur`. Saturates on overflow.
    pub fn minus(&self, dur: Duration) -> Self {
        Self(self.0 - chrono::Duration::from_std(dur).unwrap_or_else(|_| chrono::Duration::zero()))
    }

    /// Time elapsed from `self` to now, or `Duration::ZERO` if `self` is in the
    /// future.
    pub fn elapsed(&self) -> Duration {
        (Utc::now() - self.0).to_std().unwrap_or(Duration::ZERO)
    }

    /// Whole seconds elapsed from `self` to now as a signed count — the
    /// vocabulary uptime/age readouts expect. Carries the `i64` so callers
    /// never cast at the boundary.
    pub fn elapsed_seconds(&self) -> i64 {
        (Utc::now() - self.0).num_seconds().max(0)
    }
}

/// Error parsing an RFC 3339 timestamp. orca-owned so callers never name the
/// datetime library's error type.
#[derive(Debug, Clone)]
pub struct ParseError(String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid RFC 3339 timestamp: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Timestamp::parse_rfc3339(&s).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Timestamp {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Timestamp".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time",
            "description": "UTC instant as an RFC 3339 string.",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_secs_since_epoch_is_recent_and_positive() {
        let now = now_secs_since_epoch();
        assert!(now > 1_704_067_200, "got {now}");
        assert!(now < 4_102_444_800, "got {now}");
    }

    #[test]
    fn now_rfc3339_parses_back() {
        let s = now_rfc3339();
        assert!(Timestamp::parse_rfc3339(&s).is_ok(), "round-trips: {s}");
    }

    #[test]
    fn rfc3339_round_trips() {
        let t = Timestamp::parse_rfc3339("2026-07-09T18:20:05Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-07-09T18:20:05Z");
        assert_eq!(t.unix_seconds(), 1_783_621_205);
    }

    #[test]
    fn serde_is_rfc3339_string() {
        let t = Timestamp::from_unix_seconds(1_783_621_205).unwrap();
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"2026-07-09T18:20:05Z\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), t);
    }

    #[test]
    fn arithmetic_and_ordering() {
        let t = Timestamp::from_unix_seconds(1000).unwrap();
        let later = t.plus(Duration::from_secs(60));
        assert_eq!(later.unix_seconds(), 1060);
        assert!(later > t);
        assert_eq!(later.minus(Duration::from_secs(60)), t);
    }

    #[test]
    fn compact_is_sortable() {
        let t = Timestamp::parse_rfc3339("2026-07-09T18:20:05Z").unwrap();
        assert_eq!(t.compact(), "20260709-182005");
    }
}
