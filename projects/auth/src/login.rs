//! Shared operator-credential verification.
//!
//! Both the cookie `signin` HTTP handler (`server/src/serve/auth_routes.rs`)
//! and the `auth.login` tool ([`crate::auth`]) must run the identical
//! throttle-check → user-lookup → password-verify → throttle-record sequence
//! before they diverge on transport (cookie + `SESSION_TTL` vs on-disk
//! `$ORCA_HOME/session` + 24h TTL). This is the one source of truth for that
//! credential check; callers turn the [`VerifyOutcome`] into their own
//! transport-specific response.

use crate::{password, throttle, users};

/// Result of checking a username/password against the throttle bucket and the
/// `users` table. Callers map each arm to their transport (HTTP status vs
/// `anyhow::bail`). DB errors are surfaced separately as the `Err` of
/// [`verify_credentials`].
pub enum VerifyOutcome {
    /// Credentials matched; carries the full auth row (id, username, role).
    Verified(users::UserAuth),
    /// The IP/username bucket is rate-limited; wait this many seconds.
    Throttled { retry_after_secs: u64 },
    /// No such user, or the password did not match.
    Invalid,
}

/// Run the shared credential check: throttle gate, user lookup, and password
/// verification, recording the throttle success/failure as a side effect.
///
/// `ip` is the throttle bucket key ("127.0.0.1" for the CLI path, the peer IP
/// for REST signin). A database lookup error propagates as `Err`; all other
/// outcomes are encoded in [`VerifyOutcome`].
pub fn verify_credentials(
    conn: &db::Conn,
    ip: &str,
    username: &str,
    password: &str,
) -> anyhow::Result<VerifyOutcome> {
    if let throttle::CheckOutcome::Throttled { retry_after_secs } = throttle::check(ip, username) {
        return Ok(VerifyOutcome::Throttled { retry_after_secs });
    }

    let row = match users::find_auth_by_username(conn, username)? {
        Some(r) => r,
        None => {
            throttle::record_failure(ip, username);
            return Ok(VerifyOutcome::Invalid);
        }
    };

    if !password::verify_password(password, &row.password_hash).unwrap_or(false) {
        throttle::record_failure(ip, username);
        return Ok(VerifyOutcome::Invalid);
    }

    throttle::record_success(ip, username);
    Ok(VerifyOutcome::Verified(row))
}
