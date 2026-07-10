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

// Caller-token args are the tool's on-wire JSON payload — genuinely free-form
// at this boundary (hashed, not interpreted). Typed shapes are deserialized by
// the dispatcher after authorization.
#![allow(clippy::disallowed_types)]

use anyhow::{Context, Result};
use contract::CallerIdentity;
use serde::{Deserialize, Serialize};
use utils::hash;

/// Default token lifetime. Tokens are minted per request immediately before
/// dispatch, so a tight window is fine and limits replay exposure.
pub const DEFAULT_TTL_SECS: i64 = 60;

/// The signed body carried in `PodExecParams.caller_token`. Serialized to
/// canonical JSON and signed; see [`utils::pki::sign_envelope`].
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

/// Hex SHA-256 of the canonical-JSON encoding of `args`. Object keys are
/// recursively sorted before serialization so the hash is stable regardless
/// of feature unification (e.g. `serde_json/preserve_order` pulled in by
/// `oas3` via `integrations/openapi`).
pub fn args_hash(args: &serde_json::Value) -> String {
    // Canonicalize: recursively sort object keys before serializing. We can't
    // rely on `serde_json::Map`'s natural order because workspace feature
    // unification may pull in `serde_json/preserve_order` via deps like
    // `oas3` (integrations/openapi), flipping Map from BTreeMap to IndexMap.
    // Sorting here keeps the hash stable across any feature combination.
    let canonical = canonicalize(args);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    hash::sha256_hex(&bytes)
}

fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonicalize(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
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
) -> Result<utils::pki::SignedEnvelope> {
    let signing =
        utils::pki::load_or_init_bootstrap_key(pki_dir).context("load bootstrap key for token")?;
    let now = utils::time::now().unix_seconds();
    let token = CallerToken {
        caller_user_id: identity.user_id.clone(),
        caller_username: identity.username.clone(),
        role: identity.role.clone(),
        tool: tool.to_string(),
        args_hash: args_hash(args),
        issued_at: now,
        expires_at: now + ttl_secs,
        nonce: utils::id::new(),
    };
    utils::pki::sign_envelope(&signing, &token).context("sign caller token")
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
    env: &utils::pki::SignedEnvelope,
    tool: &str,
    args: &serde_json::Value,
    now: i64,
) -> Result<Verified> {
    let (token, verifying) =
        utils::pki::verify_envelope::<CallerToken>(env).context("verify caller token envelope")?;

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

    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&verifying);
    Ok(Verified { token, signer_fp })
}

/// In-memory replay guard for caller-token nonces. Tokens are short-lived
/// (`DEFAULT_TTL_SECS`), so a process-local cache is sufficient: a captured
/// token can only be replayed within its expiry window, and a daemon restart
/// drops the cache only after every cached token would already have expired.
mod replay {
    use std::collections::HashMap;
    use std::sync::Mutex;

    // nonce → expiry (unix seconds). Pruned opportunistically on each insert.
    static SEEN: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

    /// Record an unseen `nonce` (valid until `expires_at`). Returns true if it
    /// was new; false if the nonce was already used (i.e. a replay).
    pub fn record_unseen(nonce: &str, expires_at: i64, now: i64) -> bool {
        let mut guard = SEEN.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        map.retain(|_, exp| *exp > now);
        if map.contains_key(nonce) {
            return false;
        }
        map.insert(nonce.to_string(), expires_at);
        true
    }
}

/// Reject a token whose nonce has already been seen within its validity window.
pub fn check_replay(nonce: &str, expires_at: i64, now: i64) -> Result<()> {
    if !replay::record_unseen(nonce, expires_at, now) {
        anyhow::bail!("caller token nonce {nonce} was already used (replay rejected)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_pki() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn replay_guard_rejects_second_use() {
        let nonce = format!("nonce-{}", utils::id::new());
        // First use within validity → accepted.
        check_replay(&nonce, 1000, 0).unwrap();
        // Same nonce again → rejected as a replay.
        let err = check_replay(&nonce, 1000, 0).unwrap_err();
        assert!(err.to_string().contains("replay rejected"), "{err}");
    }

    #[test]
    fn replay_guard_forgets_expired_nonces() {
        let nonce = format!("nonce-{}", utils::id::new());
        check_replay(&nonce, 100, 0).unwrap();
        // Once `now` passes the expiry, the nonce is pruned and may reappear
        // (a fresh token with the same nonce is implausible, but this proves
        // the cache doesn't grow without bound).
        check_replay(&nonce, 300, 200).unwrap();
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
        let args = json!({"version": "v1", "peer": "bravo"});
        let env = mint(dir.path(), &ident(), "system.update.create", &args, 60).unwrap();
        let now = utils::time::now().unix_seconds();
        let v = verify(&env, "system.update.create", &args, now).unwrap();
        assert_eq!(v.token.caller_user_id, "u-1");
        assert_eq!(v.token.role, "admin");
        // signer fp must equal the host bootstrap key fp.
        let signing = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let expected_fp = utils::pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());
        assert_eq!(v.signer_fp, expected_fp);
    }

    #[test]
    fn verify_rejects_tool_mismatch() {
        let dir = tmp_pki();
        let args = json!({});
        let env = mint(dir.path(), &ident(), "system.update.create", &args, 60).unwrap();
        let now = utils::time::now().unix_seconds();
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
        let now = utils::time::now().unix_seconds();
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
        let now = utils::time::now().unix_seconds();
        let err = verify(&env, "system.update.create", &args, now).unwrap_err();
        assert!(err.to_string().contains("envelope"), "{err}");
    }
}
