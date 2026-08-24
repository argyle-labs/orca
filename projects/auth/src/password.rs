//! Argon2id password hashing for web-UI accounts.
//!
//! Hashes use the standard PHC encoded form
//! `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`. Stored as-is in
//! `users.password_hash`; verification re-parses the encoded form so
//! parameters and salts are paired with the hash they produced.

use anyhow::Result;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};

/// True only in a debug/test build with `ORCA_TEST_FAST_KDF` set. Release
/// builds compile `cfg!(debug_assertions)` to `false`, so this is a hard `false`
/// in production regardless of the environment — the env var can never weaken a
/// shipped binary. Test/dev builds honor it to skip the memory-hard KDF cost.
fn fast_test_kdf() -> bool {
    cfg!(debug_assertions) && std::env::var_os("ORCA_TEST_FAST_KDF").is_some()
}

/// OWASP-2024 recommended argon2id parameters for interactive auth
/// (m=19 MiB, t=2 iters, p=1 lane). Under `ORCA_TEST_FAST_KDF` (debug/test only)
/// the cheapest valid params are used instead — cost-only: algorithm, PHC
/// encoded format, and the hash/verify roundtrip are identical; test hashes
/// embed the cheap params so verify re-parses them and stays cheap too. This
/// keeps the auth suite (many hashes/verifies under coverage instrumentation)
/// from spending tens of seconds per test in the KDF. Production is unaffected.
fn argon() -> Argon2<'static> {
    if fast_test_kdf() {
        use argon2::{Algorithm, Params, Version};
        return Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8, 1, 1, None).expect("valid cheap test argon2 params"),
        );
    }
    Argon2::default()
}

pub fn hash_password(plaintext: &str) -> Result<String> {
    // Use the workspace's `rand` to fill 16 random salt bytes (avoids pinning
    // the argon2 crate's internal rand_core version).
    use rand::Rng;
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt =
        SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("encode salt: {e}"))?;
    Ok(argon()
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash: {e}"))?
        .to_string())
}

/// Constant-time verify. Returns `Ok(true)` iff the password matches.
/// `Ok(false)` for a mismatch, `Err` if the stored hash is malformed.
pub fn verify_password(plaintext: &str, encoded_hash: &str) -> Result<bool> {
    let parsed = PasswordHash::new(encoded_hash)
        .map_err(|e| anyhow::anyhow!("parse stored password hash: {e}"))?;
    Ok(argon()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_mismatch() {
        let h = hash_password("hunter2").unwrap();
        assert!(verify_password("hunter2", &h).unwrap());
        assert!(!verify_password("hunter3", &h).unwrap());
    }

    #[test]
    fn malformed_hash_errors() {
        assert!(verify_password("x", "not-a-phc-string").is_err());
    }
}
