//! Signed caller token for `mesh/exec` (S1 of remote-exec-full-fix).
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
//!     pinned `mesh_peers.pubkey_fp` (verified by the recipient, not here);
//!   - anti-tamper: the signature covers `tool` + `args_hash`;
//!   - binding: a bound envelope (method `mesh/exec`) names the sender and the
//!     target member, so a token cannot be replayed against another member;
//!   - anti-replay: the nonce is recorded in the persistent mesh nonce store;
//!   - expiry: the binding's `expires_at` bounds a captured token's use.
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

pub const METHOD: &str = "mesh/exec";

/// The claims a caller token binds: who the call is for and exactly which
/// tool and args it authorizes. Carried as the payload of a bound envelope
/// (method `mesh/exec`, sender = calling member, receiver = target member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerClaims {
    pub caller_user_id: String,
    pub caller_username: String,
    /// Advisory: the recipient caps it with its own replicated `users` row.
    pub role: String,
    pub tool: String,
    pub args_hash: String,
}

/// Token shape sent by daemons that predate bound envelopes. Accepted only
/// while [`crate::mesh::trust_registry::unbound_allowed`] holds for the peer.
#[derive(Debug, Clone, Deserialize)]
struct LegacyCallerToken {
    caller_user_id: String,
    caller_username: String,
    role: String,
    tool: String,
    args_hash: String,
    expires_at: i64,
    nonce: String,
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

/// Mint a caller token for `tool`+`args` on behalf of `identity`, bound from
/// `sender` to `receiver` for `ttl_secs`. Signed with the host bootstrap key.
#[allow(clippy::too_many_arguments)]
pub fn mint(
    pki_dir: &std::path::Path,
    identity: &CallerIdentity,
    tool: &str,
    args: &serde_json::Value,
    sender: &str,
    receiver: &str,
    now: i64,
    ttl_secs: i64,
) -> Result<utils::pki::SignedEnvelope> {
    let signing =
        utils::pki::load_or_init_bootstrap_key(pki_dir).context("load bootstrap key for token")?;
    let claims = CallerClaims {
        caller_user_id: identity.user_id.clone(),
        caller_username: identity.username.clone(),
        role: identity.role.clone(),
        tool: tool.to_string(),
        args_hash: args_hash(args),
    };
    utils::pki::sign_bound_envelope(
        &signing,
        &claims,
        METHOD,
        sender,
        receiver,
        &utils::id::new(),
        now,
        ttl_secs,
    )
    .context("sign caller token")
}

/// A verified token: its claims, the nonce and expiry to replay-check, and
/// the signer fp to match against the calling peer's pinned key.
#[derive(Debug)]
pub struct Verified {
    pub claims: CallerClaims,
    pub nonce: String,
    pub expires_at: i64,
    pub signer_fp: String,
}

/// Verify a token from `sender` to `receiver` for exactly `tool` and `args`.
/// An unbound token is accepted only when `allow_unbound` (a pre-binding
/// peer during rollout).
pub fn verify(
    env: &utils::pki::SignedEnvelope,
    tool: &str,
    args: &serde_json::Value,
    sender: &str,
    receiver: &str,
    now: i64,
    allow_unbound: bool,
) -> Result<Verified> {
    let (claims, nonce, expires_at, verifying) = if env.binding.is_some() {
        let (claims, verifying, binding) =
            utils::pki::verify_bound_envelope::<CallerClaims>(env, METHOD, sender, receiver, now)
                .context("verify caller token envelope")?;
        (claims, binding.nonce.clone(), binding.expires_at, verifying)
    } else {
        anyhow::ensure!(
            allow_unbound,
            "caller token is not bound to a receiver and nonce"
        );
        let (legacy, verifying) = utils::pki::verify_envelope::<LegacyCallerToken>(env)
            .context("verify caller token envelope")?;
        anyhow::ensure!(
            now < legacy.expires_at,
            "caller token expired at {} (now {now})",
            legacy.expires_at
        );
        let claims = CallerClaims {
            caller_user_id: legacy.caller_user_id,
            caller_username: legacy.caller_username,
            role: legacy.role,
            tool: legacy.tool,
            args_hash: legacy.args_hash,
        };
        (claims, legacy.nonce, legacy.expires_at, verifying)
    };
    anyhow::ensure!(
        claims.tool == tool,
        "caller token tool mismatch: token authorizes '{}' but request is '{tool}'",
        claims.tool
    );
    anyhow::ensure!(
        claims.args_hash == args_hash(args),
        "caller token args_hash mismatch: payload was tampered or re-encoded"
    );
    Ok(Verified {
        claims,
        nonce,
        expires_at,
        signer_fp: utils::pki::bootstrap_pubkey_fingerprint(&verifying),
    })
}

/// Reject a token whose nonce `sender` already used. Nonces persist in the
/// mesh nonce store, so a restart does not reopen replay.
pub fn check_replay(
    conn: &rusqlite::Connection,
    sender: &str,
    nonce: &str,
    expires_at: i64,
    now: i64,
) -> Result<()> {
    if !db::mesh::record_mesh_nonce(conn, METHOD, sender, nonce, expires_at, now)? {
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

    const SENDER: &str = "019f0000-0000-7000-8000-00000000000a";
    const RECEIVER: &str = "019f0000-0000-7000-8000-00000000000b";
    const NOW: i64 = 1_800_000_000;

    fn replay_db() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::open_unencrypted(&dir.path().join("orca.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn a_caller_token_nonce_stays_spent_across_a_restart() {
        let (dir, conn) = replay_db();
        let nonce = utils::id::new();
        check_replay(&conn, SENDER, &nonce, NOW + 60, NOW).unwrap();
        drop(conn);
        let restarted = db::open_unencrypted(&dir.path().join("orca.db")).unwrap();
        let err = check_replay(&restarted, SENDER, &nonce, NOW + 60, NOW + 10).unwrap_err();
        assert!(err.to_string().contains("replay rejected"), "{err}");
    }

    fn ident() -> CallerIdentity {
        CallerIdentity {
            user_id: "u-1".into(),
            username: "scott".into(),
            role: "admin".into(),
            can_mutate: false,
        }
    }

    fn minted(
        dir: &std::path::Path,
        tool: &str,
        args: &serde_json::Value,
    ) -> utils::pki::SignedEnvelope {
        mint(dir, &ident(), tool, args, SENDER, RECEIVER, NOW, 60).unwrap()
    }

    fn check(
        env: &utils::pki::SignedEnvelope,
        tool: &str,
        args: &serde_json::Value,
        receiver: &str,
        now: i64,
    ) -> Result<Verified> {
        verify(env, tool, args, SENDER, receiver, now, false)
    }

    #[test]
    fn args_hash_is_stable_for_equal_values() {
        let a = json!({"b": 2, "a": 1});
        let b = json!({"a": 1, "b": 2});
        assert_eq!(args_hash(&a), args_hash(&b));
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let dir = tmp_pki();
        let args = json!({"version": "v1", "peer": "bravo"});
        let env = minted(dir.path(), "system.update.create", &args);
        let v = check(&env, "system.update.create", &args, RECEIVER, NOW).unwrap();
        assert_eq!(v.claims.caller_user_id, "u-1");
        assert_eq!(v.claims.role, "admin");
        let signing = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        assert_eq!(
            v.signer_fp,
            utils::pki::bootstrap_pubkey_fingerprint(&signing.verifying_key())
        );
    }

    #[test]
    fn verify_rejects_tool_mismatch() {
        let dir = tmp_pki();
        let env = minted(dir.path(), "system.update.create", &json!({}));
        let err = check(&env, "system.mesh.delete", &json!({}), RECEIVER, NOW).unwrap_err();
        assert!(err.to_string().contains("tool mismatch"), "{err}");
    }

    #[test]
    fn verify_rejects_args_tamper() {
        let dir = tmp_pki();
        let env = minted(dir.path(), "system.update.create", &json!({"v": 1}));
        let err = check(
            &env,
            "system.update.create",
            &json!({"v": 2}),
            RECEIVER,
            NOW,
        )
        .unwrap_err();
        assert!(err.to_string().contains("args_hash mismatch"), "{err}");
    }

    #[test]
    fn verify_rejects_expired_and_another_receiver() {
        let dir = tmp_pki();
        let env = minted(dir.path(), "t", &json!({}));
        let err = check(&env, "t", &json!({}), RECEIVER, NOW + 61).unwrap_err();
        assert!(format!("{err:#}").contains("expired"), "{err:#}");
        let err = check(&env, "t", &json!({}), SENDER, NOW).unwrap_err();
        assert!(format!("{err:#}").contains("another receiver"), "{err:#}");
    }

    #[test]
    fn verify_rejects_forged_claims() {
        let dir = tmp_pki();
        let mut env = minted(dir.path(), "t", &json!({}));
        let mut claims: CallerClaims = serde_json::from_str(&env.payload).unwrap();
        claims.caller_user_id = "u-evil".into();
        env.payload = serde_json::to_string(&claims).unwrap();
        let err = check(&env, "t", &json!({}), RECEIVER, NOW).unwrap_err();
        assert!(format!("{err:#}").contains("envelope"), "{err:#}");
    }

    #[test]
    fn an_unbound_token_is_accepted_only_from_a_pre_binding_peer() {
        let dir = tmp_pki();
        let signing = utils::pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let legacy = json!({
            "caller_user_id": "u-1", "caller_username": "scott", "role": "admin",
            "tool": "t", "args_hash": args_hash(&json!({})), "issued_at": NOW,
            "expires_at": NOW + 60, "nonce": utils::id::new(),
        });
        let env = utils::pki::sign_envelope(&signing, &legacy).unwrap();
        let err = check(&env, "t", &json!({}), RECEIVER, NOW).unwrap_err();
        assert!(err.to_string().contains("not bound"), "{err}");
        verify(&env, "t", &json!({}), SENDER, RECEIVER, NOW, true).unwrap();
    }
}
