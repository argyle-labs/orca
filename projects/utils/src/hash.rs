//! Content hashing — the one place in the workspace that knows how to
//! compute SHA-256 and BLAKE3 digests. **Every callsite that used to inline
//! `Sha256::new()` should call through here** (see
//! `project_crate_audit_2026_05_29` P1 #3). Lives at top-level
//! `utils::hash` (not under `utils::fs`) because the algorithm is
//! generic — files are just one possible input.
//!
//! Picking a flavor:
//! - SHA-256 — interop default, matches `sha256sum` etc. Use for any
//!   bytes you also expect to verify outside orca (download checksums,
//!   token hashes, pairing-code hashes, args-hash bound into caller tokens).
//! - BLAKE3 — orca-internal content addressing where speed matters and
//!   nothing external needs to recompute the digest.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const READ_BUF: usize = 64 * 1024;

/// Raw 32-byte SHA-256 digest of `bytes`. Use when downstream code needs
/// the digest bytes themselves (signing, comparing against an array).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Lowercase-hex SHA-256 of `bytes` (64 chars). The default for storing in
/// the DB, comparing against external checksums, or printing.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&sha256(bytes))
}

/// Lowercase-hex SHA-256 of the file at `path`, streamed through a 64 KiB
/// buffer — safe for arbitrarily large files.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut r = BufReader::new(f);
    let mut buf = [0u8; READ_BUF];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex_encode(&h.finalize()))
}

/// Lowercase-hex BLAKE3 of the file at `path`.
pub fn blake3_file(path: &Path) -> Result<String> {
    let mut h = blake3::Hasher::new();
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut r = BufReader::new(f);
    let mut buf = [0u8; READ_BUF];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Lowercase-hex encode a byte slice. Exposed so callers needing only the
/// hex step (e.g. random-token serialization) don't reinvent it.
pub fn hex_encode(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        write!(s, "{byte:02x}").expect("write to String is infallible");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    const HELLO_SHA256_HEX: &str =
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(sha256_hex(b"hello"), HELLO_SHA256_HEX);
    }

    #[test]
    fn sha256_raw_round_trips_through_hex_encode() {
        let raw = sha256(b"hello");
        assert_eq!(hex_encode(&raw), HELLO_SHA256_HEX);
    }

    #[test]
    fn sha256_file_streams_files() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"hello").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), HELLO_SHA256_HEX);
    }

    #[test]
    fn blake3_streams_files() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"").unwrap();
        // blake3("") = af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
        assert_eq!(
            blake3_file(&p).unwrap(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hex_encode_zero_pads_low_bytes() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
