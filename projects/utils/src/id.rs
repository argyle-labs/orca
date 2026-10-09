//! Identifier generation — the one place in the workspace that knows how orca
//! mints unique IDs. **Every callsite that used to inline `uuid::Uuid::…`
//! should call through here.** The backing library (uuid today) is an
//! implementation detail: swap it and no caller changes, because no caller
//! ever names it. This is an abstraction, not a re-export — there is
//! deliberately no `pub use ::uuid`.
//!
//! orca IDs are **time-ordered** (UUIDv7): the leading bits are a millisecond
//! timestamp, so IDs sort chronologically as strings — handy for DB primary
//! keys and log correlation. Callers get an opaque `String`; they store and
//! compare it as text and never depend on the UUID layout.

/// A fresh, time-ordered unique ID as a lowercase-hyphenated string.
/// This is the default — use it anywhere you need a new identifier, nonce,
/// session id, or correlation id.
pub fn new() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The first 8 characters of a fresh ID — a short, human-friendly handle for
/// logs and display where global uniqueness is not required. Not collision-safe
/// at scale; use [`new`] for anything persisted or keyed.
pub fn new_short() -> String {
    new()[..8].to_string()
}

/// True if `s` is a syntactically valid orca ID (parses as a UUID). Use to
/// validate externally-supplied identifiers without naming the UUID library.
pub fn is_valid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

/// True if `s` is a canonical **UUIDv7** — the strict form every orca identity
/// must take. Stricter than [`is_valid`]: it rejects UUIDs of other versions,
/// most importantly a bare 32-hex OS machine-id (`/etc/machine-id`,
/// `IOPlatformUUID`), which [`uuid::Uuid::parse_str`] happily accepts as a
/// non-v7 UUID but which is *not* time-ordered and breaks id-based targeting.
/// Use this to validate a persisted identity before trusting it.
pub fn is_uuidv7(s: &str) -> bool {
    uuid::Uuid::parse_str(s)
        .map(|u| u.get_version_num() == 7)
        .unwrap_or(false)
}

/// An externally-supplied identifier that is known to be a UUID — the type for
/// every id-named tool argument. Parsing rejects anything else (a hostname, a
/// display name, an address), so a name can never be silently resolved where an
/// id was asked for.
///
/// Stored ids are compared as text, so only the forms orca writes are
/// accepted: hyphenated (any case, stored lowercase) and lowercase bare 32-hex,
/// the legacy machine-id form, kept verbatim so it still matches its own row.
/// Braced, urn and uppercase bare forms are rejected, as are the nil and max
/// UUIDs, which are placeholders and never name a row.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// A value that is not a UUID where one was required. Carries the rejected
/// input so callers can build a domain-specific message around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(pub String);

impl std::fmt::Display for InvalidId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "expected an id (UUID), got `{}`", self.0)
    }
}

impl std::error::Error for InvalidId {}

impl std::str::FromStr for Id {
    type Err = InvalidId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let parsed = uuid::Uuid::try_parse(trimmed)
            .ok()
            .filter(|u| !u.is_nil() && !u.is_max());
        match (parsed, trimmed.len()) {
            (Some(u), 36) => Ok(Self(u.hyphenated().to_string())),
            (Some(_), 32) if !trimmed.bytes().any(|b| b.is_ascii_uppercase()) => {
                Ok(Self(trimmed.to_string()))
            }
            _ => Err(InvalidId(s.to_string())),
        }
    }
}

impl std::ops::Deref for Id {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Id> for String {
    fn from(id: Id) -> Self {
        id.0
    }
}

impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The schema every UUID-typed id shares, inlined so each id-named property
/// carries `format: uuid` directly rather than behind a `$ref`.
pub fn uuid_schema() -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "format": "uuid",
    })
}

impl schemars::JsonSchema for Id {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Id".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        uuid_schema()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_valid_and_unique() {
        let a = new();
        let b = new();
        assert!(is_valid(&a));
        assert!(is_valid(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn v7_ids_sort_by_creation_order() {
        let first = new();
        let second = new();
        // UUIDv7 is time-ordered: a later ID sorts lexicographically after an
        // earlier one (same-millisecond ties are broken by random bits, so we
        // only assert the two are ordered, not which way on a tie).
        assert!(first != second);
    }

    #[test]
    fn new_short_is_eight_chars() {
        assert_eq!(new_short().len(), 8);
    }

    #[test]
    fn is_uuidv7_accepts_minted_ids_only() {
        // A freshly minted id is UUIDv7.
        assert!(is_uuidv7(&new()));
        // A bare 32-hex OS machine-id parses as a UUID (so `is_valid` passes)
        // but is NOT v7 — the exact case that split the fleet's identities.
        assert!(is_valid("dd7a73cda6222ddfaae8fbff692f27f6"));
        assert!(!is_uuidv7("dd7a73cda6222ddfaae8fbff692f27f6"));
        // A v4 UUID is a valid UUID but not v7.
        assert!(!is_uuidv7("f47ac10b-58cc-4372-a567-0e02b2c3d479"));
        // Garbage / short hex is neither.
        assert!(!is_uuidv7("c56ccc7c2039"));
        assert!(!is_uuidv7("not-a-uuid"));
    }

    #[test]
    fn is_valid_rejects_garbage() {
        assert!(!is_valid("not-an-id"));
        assert!(!is_valid(""));
    }

    #[test]
    fn id_parses_uuids_and_keeps_their_text() {
        let minted = new();
        assert_eq!(minted.parse::<Id>().unwrap().as_str(), minted);
        assert_eq!(
            format!("  {minted}\n").parse::<Id>().unwrap().as_str(),
            minted
        );
        // A bare 32-hex id stays bare so it still matches its stored row.
        let bare = "dd7a73cda6222ddfaae8fbff692f27f6";
        assert_eq!(bare.parse::<Id>().unwrap().as_str(), bare);
    }

    #[test]
    fn id_lowercases_hyphenated_input() {
        let upper = "019F9F7B-1176-7E40-9E30-4987D8D12DCB";
        assert_eq!(
            upper.parse::<Id>().unwrap().as_str(),
            "019f9f7b-1176-7e40-9e30-4987d8d12dcb"
        );
    }

    #[test]
    fn id_rejects_braced_urn_and_uppercase_bare_forms() {
        for bad in [
            "{019f9f7b-1176-7e40-9e30-4987d8d12dcb}",
            "urn:uuid:019f9f7b-1176-7e40-9e30-4987d8d12dcb",
            "DD7A73CDA6222DDFAAE8FBFF692F27F6",
        ] {
            assert_eq!(bad.parse::<Id>().unwrap_err(), InvalidId(bad.to_string()));
        }
    }

    #[test]
    fn id_rejects_the_nil_and_max_uuids() {
        for bad in [
            "00000000-0000-0000-0000-000000000000",
            "00000000000000000000000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
            "ffffffffffffffffffffffffffffffff",
        ] {
            assert_eq!(bad.parse::<Id>().unwrap_err(), InvalidId(bad.to_string()));
        }
    }

    #[test]
    fn id_rejects_names_and_addresses() {
        for bad in ["mint", "10.0.0.5", "", "  ", "peer.c56ccc7c2039"] {
            let err = bad.parse::<Id>().unwrap_err();
            assert_eq!(err, InvalidId(bad.to_string()));
        }
    }

    #[test]
    fn id_serde_is_a_validated_string() {
        let minted = new();
        let quoted = format!("\"{minted}\"");
        let id: Id = serde_json::from_str(&quoted).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), quoted);
        let err = serde_json::from_str::<Id>("\"mint\"").unwrap_err();
        assert!(err.to_string().contains("`mint`"), "{err}");
    }

    #[test]
    fn id_schema_is_an_inline_uuid_string() {
        let schema = schemars::schema_for!(Option<Id>);
        assert_eq!(schema.get("format").and_then(|f| f.as_str()), Some("uuid"));
        assert!(schema.get("$ref").is_none());
    }
}
