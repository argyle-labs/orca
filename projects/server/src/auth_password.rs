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

/// OWASP-2024 recommended argon2id parameters for interactive auth
/// (m=19 MiB, t=2 iters, p=1 lane).
fn argon() -> Argon2<'static> {
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
