//! Base64 encoding — the one place in the workspace that knows how orca
//! base64-encodes bytes. **Every callsite that used to inline
//! `base64::engine::…` should call through here.** The backing library
//! (base64 today) is hidden: no caller names its `Engine` trait or engine
//! constants. This is an abstraction, not a re-export.
//!
//! Two alphabets, matching the two things orca actually needs:
//! - **standard** (`+`/`/`, padded) — the interop default, matches `base64`
//!   CLI output and most wire formats (signatures, embedded blobs).
//! - **url-safe, no padding** (`-`/`_`, unpadded) — for tokens carried in URLs
//!   / OAuth `code_verifier` challenges, where `+`/`/`/`=` are unwelcome.

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// Standard base64 (padded) encode.
pub fn base64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Standard base64 (padded) decode. Errors on invalid input.
pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    STANDARD.decode(s).context("base64 decode")
}

/// URL-safe base64, no padding — encode.
pub fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// URL-safe base64, no padding — decode. Errors on invalid input.
pub fn base64url_decode(s: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).context("base64url decode")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_round_trips() {
        let data = b"hello, orca";
        let enc = base64_encode(data);
        assert_eq!(enc, "aGVsbG8sIG9yY2E=");
        assert_eq!(base64_decode(&enc).unwrap(), data);
    }

    #[test]
    fn url_safe_has_no_padding() {
        // 0xFB 0xFF encodes to "+/" under standard, "-_" under url-safe.
        let enc = base64url_encode(&[0xfb, 0xff]);
        assert_eq!(enc, "-_8");
        assert!(!enc.contains('='));
        assert_eq!(base64url_decode(&enc).unwrap(), vec![0xfb, 0xff]);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(base64_decode("!!!not base64!!!").is_err());
    }
}
