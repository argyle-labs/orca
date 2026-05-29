//! Signed caller token for `pod/exec` (S1 of remote-exec-full-fix).
//!
//! The mesh mTLS chain proves which *peer* is on the wire, but not which
//! *user* that peer is acting for. Today the calling side just asserts a role
//! string and the recipient trusts it. This module replaces the bare assertion
//! with an Ed25519-signed token, minted by the calling peer's bootstrap key,
//! that binds the request to a specific user, tool, and argument set with an
//! expiry and a replay nonce.
//!
//! Trust model: the signing key is per-*peer* (the bootstrap key), not
//! per-user — per-user keypairs are out of scope. So the token does not defend
//! against a fully compromised peer (which already holds mesh mTLS certs).
//! What it does provide:
//!   - authenticated origin: the signer fp must match the authenticated peer's
//!     pinned `pod_peers.pubkey_fp` (verified by the recipient, not here);
//!   - anti-tamper: the signature covers `tool` + `args_hash`;
//!   - anti-replay: a random `nonce` the recipient tracks in a short window;
//!   - expiry: `expires_at` bounds the window a captured token is usable.
//!
//! The recipient still derives the *effective* role from its own replicated
//! `users` table keyed by `caller_user_id` (S2/S3) — the `role` field here is
//! advisory only and is never trusted for the authorization decision.

use anyhow::{Context, Result};
use orca_contract::CallerIdentity;
use orca_sdk::pki;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default token lifetime. Tokens are minted per request immediately before
/// dispatch, so a tight window is fine and limits replay exposure.
pub const DEFAULT_TTL_SECS: i64 = 60;

/// The signed body carried in `PodExecParams.caller_token`. Serialized to
/// canonical JSON and signed; see [`pki::sign_envelope`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerToken {
    /// Stable id of the user this call is made on behalf of. The recipient
    /// looks this up in its replicated `users` table to derive the role.
    pub caller_user_id: String,
    /// Username, advisory (for logs / error messages).
    pub caller_username: String,
    /// Role the caller *asserts*. Advisory only — never used for the
    /// authorization decision (recipient uses the replicated users table).
    pub role: String,
    /// Tool the token authorizes. Recipient rejects if it differs from the
    /// requested tool.
    pub tool: String,
    /// Hex SHA-256 of the canonical-JSON args. Binds the token to its payload.
    pub args_hash: String,
    /// Unix seconds when minted.
    pub issued_at: i64,
    /// Unix seconds after which the token is rejected.
    pub expires_at: i64,
    /// Random per-token nonce for replay detection.
    pub nonce: String,
}

/// Hex SHA-256 of the canonical-JSON encoding of `args`. Both peers derive the
/// `serde_json::Value` from the same wire JSON, and `serde_json::Map` is a
/// sorted `BTreeMap` (no `preserve_order` feature), so the re-serialized bytes
/// match on both ends.
pub fn args_hash(args: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(args).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(&bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d.iter() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Synthetic identity for host-internal system operations (e.g. the mutual
/// pod-trust push) that aren't driven by a logged-in user. `caller_user_id`
/// will not resolve in the recipient's replicated `users` table, so the
/// recipient falls back to the trusted-peer path (signer fp pinned to a paired
/// peer) and honors the asserted admin role.
pub fn system_operator() -> CallerIdentity {
    CallerIdentity {
        user_id: "system".to_string(),
        username: "system".to_string(),
        role: "admin".to_string(),
    }
}

/// Mint and sign a token for `tool`+`args` on behalf of `identity`, valid for
/// `ttl_secs`. Signed with the host bootstrap key (`pki_dir`).
pub fn mint(
    pki_dir: &std::path::Path,
    identity: &CallerIdentity,
    tool: &str,
    args: &serde_json::Value,
    ttl_secs: i64,
) -> Result<pki::SignedEnvelope> {
    let signing =
        pki::load_or_init_bootstrap_key(pki_dir).context("load bootstrap key for token")?;
    let now = chrono::Utc::now().timestamp();
    let token = CallerToken {
        caller_user_id: identity.user_id.clone(),
        caller_username: identity.username.clone(),
        role: identity.role.clone(),
        tool: tool.to_string(),
        args_hash: args_hash(args),
        issued_at: now,
        expires_at: now + ttl_secs,
        nonce: uuid::Uuid::now_v7().to_string(),
    };
    pki::sign_envelope(&signing, &token).context("sign caller token")
}

/// Outcome of verifying a token's *self-contained* claims (signature, expiry,
/// tool binding, args binding). The caller is responsible for the two checks
/// this function cannot do on its own: matching `signer_fp` against the
/// authenticated peer's pinned fp, and replay-checking `token.nonce`.
#[derive(Debug)]
pub struct Verified {
    pub token: CallerToken,
    /// Bootstrap-pubkey fingerprint of the signer, to be matched against the
    /// authenticated peer's pinned `pod_peers.pubkey_fp`.
    pub signer_fp: String,
}

/// Verify the envelope signature and the self-contained claims: `tool` matches,
/// `args_hash` matches `args`, and `now` is before `expires_at`. Returns the
/// decoded token plus the signer fp for the caller's peer-binding + replay
/// checks. Does NOT consult the users table or any replay cache.
pub fn verify(
    env: &pki::SignedEnvelope,
    tool: &str,
    args: &serde_json::Value,
    now: i64,
) -> Result<Verified> {
    let (token, verifying) =
        pki::verify_envelope::<CallerToken>(env).context("verify caller token envelope")?;

    if token.tool != tool {
        anyhow::bail!(
            "caller token tool mismatch: token authorizes '{}' but request is '{tool}'",
            token.tool
        );
    }
    let expected = args_hash(args);
    if token.args_hash != expected {
        anyhow::bail!("caller token args_hash mismatch: payload was tampered or re-encoded");
    }
    if now >= token.expires_at {
        anyhow::bail!("caller token expired at {} (now {now})", token.expires_at);
    }

    let signer_fp = pki::bootstrap_pubkey_fingerprint(&verifying);
    Ok(Verified { token, signer_fp })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_pki() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn ident() -> CallerIdentity {
        CallerIdentity {
            user_id: "u-1".into(),
            username: "scott".into(),
            role: "admin".into(),
        }
    }

    #[test]
    fn args_hash_is_stable_for_equal_values() {
        let a = json!({"b": 2, "a": 1});
        let b = json!({"a": 1, "b": 2});
        // serde_json Map is sorted, so logically-equal objects hash equal
        // regardless of literal key order.
        assert_eq!(args_hash(&a), args_hash(&b));
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let dir = tmp_pki();
        let args = json!({"version": "v1", "peer": "baldur"});
        let env = mint(dir.path(), &ident(), "system.update.create", &args, 60).unwrap();
        let now = chrono::Utc::now().timestamp();
        let v = verify(&env, "system.update.create", &args, now).unwrap();
        assert_eq!(v.token.caller_user_id, "u-1");
        assert_eq!(v.token.role, "admin");
        // signer fp must equal the host bootstrap key fp.
        let signing = pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let expected_fp = pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());
        assert_eq!(v.signer_fp, expected_fp);
    }

    #[test]
    fn verify_rejects_tool_mismatch() {
        let dir = tmp_pki();
        let args = json!({});
        let env = mint(dir.path(), &ident(), "system.update.create", &args, 60).unwrap();
        let now = chrono::Utc::now().timestamp();
        let err = verify(&env, "pod.kick", &args, now).unwrap_err();
        assert!(err.to_string().contains("tool mismatch"), "{err}");
    }

    #[test]
    fn verify_rejects_args_tamper() {
        let dir = tmp_pki();
        let env = mint(
            dir.path(),
            &ident(),
            "system.update.create",
            &json!({"v": 1}),
            60,
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let err = verify(&env, "system.update.create", &json!({"v": 2}), now).unwrap_err();
        assert!(err.to_string().contains("args_hash mismatch"), "{err}");
    }

    #[test]
    fn verify_rejects_expired() {
        let dir = tmp_pki();
        let args = json!({});
        let env = mint(dir.path(), &ident(), "system.update.create", &args, 60).unwrap();
        let token: CallerToken = serde_json::from_str(&env.payload).unwrap();
        let err = verify(&env, "system.update.create", &args, token.expires_at + 1).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn verify_rejects_forged_signature() {
        let dir = tmp_pki();
        let args = json!({});
        let mut env = mint(dir.path(), &ident(), "system.update.create", &args, 60).unwrap();
        // Flip the payload but keep the old signature → signature must fail.
        let mut token: CallerToken = serde_json::from_str(&env.payload).unwrap();
        token.role = "admin".into();
        token.caller_user_id = "u-evil".into();
        env.payload = serde_json::to_string(&token).unwrap();
        let now = chrono::Utc::now().timestamp();
        let err = verify(&env, "system.update.create", &args, now).unwrap_err();
        assert!(err.to_string().contains("envelope"), "{err}");
    }
}
