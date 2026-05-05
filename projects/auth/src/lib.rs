//! Auth helpers — key formatting and validation.
//!
//! API key storage lives in the encrypted orca DB (`orca-db` crate, settings
//! table under the `secrets.` prefix). Read/write through `db::secret_get/set`.
//! No keychain, no plaintext config files.

/// Mask an API key for display: first 8 + ellipsis + last 4. Short keys
/// (≤ 12 chars) are fully masked since there isn't enough entropy to safely
/// echo any portion.
pub fn mask_key(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…{}", &key[..8], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}

/// Anthropic API keys begin with `sk-ant-`. Returns true for plausibly valid
/// keys; this is a sanity check, not authentication.
pub fn looks_like_anthropic_key(key: &str) -> bool {
    key.starts_with("sk-ant-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_long_key_shows_first_and_last() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let masked = mask_key(key);
        assert!(masked.starts_with("sk-ant-a"), "prefix wrong: {masked}");
        assert!(masked.ends_with("wxyz"), "suffix wrong: {masked}");
        assert!(masked.contains('…'), "no ellipsis: {masked}");
    }

    #[test]
    fn mask_key_short_returns_stars() {
        assert_eq!(mask_key("short"), "****");
    }

    #[test]
    fn mask_key_exactly_12_returns_stars() {
        assert_eq!(mask_key("abcdefghijkl"), "****");
    }

    #[test]
    fn mask_key_13_chars_masks() {
        let key = "abcdefghijklm";
        let masked = mask_key(key);
        assert!(masked.starts_with("abcdefgh"), "got: {masked}");
        assert!(masked.ends_with("jklm"), "got: {masked}");
    }

    #[test]
    fn mask_key_empty_returns_stars() {
        assert_eq!(mask_key(""), "****");
    }

    #[test]
    fn looks_like_anthropic_accepts_real_format() {
        assert!(looks_like_anthropic_key("sk-ant-api03-xyz"));
        assert!(!looks_like_anthropic_key("sk-1234"));
        assert!(!looks_like_anthropic_key(""));
    }
}
