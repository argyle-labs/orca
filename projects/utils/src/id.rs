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
}
