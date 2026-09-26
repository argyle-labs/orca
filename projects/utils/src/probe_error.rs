//! Tells "this peer is still restarting" apart from "this peer will never answer".
//!
//! A health gate that polls a peer after applying an update has to retry through
//! the restart window — the peer is legitimately unreachable for a few seconds
//! while it comes back. The trap is treating *every* probe failure as that case:
//! a permanent failure (the verb no longer exists, the credential is rejected)
//! then burns the whole gate timeout and reports "timed out" with no cause.
//!
//! Measured on the 2026-09-25 `v0.2.1-rc.3` roll: 7 of 7 hosts timed out their
//! 180s gate. The controller was on rc.2 while peers moved to rc.3, which
//! dissolved the `pod.*` verbs into `system.*`, so every probe came back
//! `unknown tool: pod.list` — permanent, and unretryable. The gate degraded into
//! a fixed 180s sleep, which is the between-host safety check from
//! [[orca-must-never-bring-down-host]] not actually checking anything.
//!
//! Pure string classification, deliberately: the polling loop and the transport
//! are IO, while "is this worth retrying" is a decision — and the decision was
//! the part that was wrong.

/// Whether a failed probe is worth retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Expected during a restart — keep polling until the gate's deadline.
    Transient,
    /// Retrying cannot help. The gate should fail immediately and report why.
    Permanent,
}

/// Substrings that mark a probe failure as unretryable. Matched
/// case-insensitively against the full error chain text.
///
/// Deliberately narrow. Every entry names something a restart cannot fix: the
/// verb is gone from the peer's build, or the peer actively refused us. Anything
/// broader risks aborting a healthy roll on a transport blip, which is the worse
/// failure — see [`classify`] on why the default is `Transient`.
const PERMANENT_MARKERS: &[&str] = &[
    // The exact class measured on the rc.2 -> rc.3 flip: the peer's build has no
    // such verb. No amount of waiting adds it back.
    "unknown tool",
    "unsupported verb",
    "unknown verb",
    "no such tool",
    // Active refusal. The peer answered — it just said no.
    "unauthorized",
    "forbidden",
    "permission denied",
    "not allowed",
    // Identity/trust problems that need an operator action, not time.
    "no pinned bootstrap key",
    "unknown peer",
];

/// Classify a probe error by its text.
///
/// Defaults to [`ProbeOutcome::Transient`] for anything unrecognized. An
/// unrecognized error is NOT evidence of a permanent fault — and the two
/// mistakes are not symmetric. Misjudging a transient error as permanent aborts
/// the gate on a healthy host and can strand a mid-roll fleet; misjudging a
/// permanent one as transient costs the gate timeout, which is what already
/// happens today. So unknown retries, and only a named marker fails fast.
pub fn classify(err: &str) -> ProbeOutcome {
    let lower = err.to_ascii_lowercase();
    if PERMANENT_MARKERS.iter().any(|m| lower.contains(m)) {
        ProbeOutcome::Permanent
    } else {
        ProbeOutcome::Transient
    }
}

/// Convenience over anything `Display` — the shape errors arrive in.
pub fn classify_err<E: std::fmt::Display>(err: &E) -> ProbeOutcome {
    classify(&err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact error behind the 7-of-7 gate timeout on 2026-09-25. If this
    /// stops classifying as Permanent, the gate goes back to sleeping 180s per
    /// host on a version flip.
    #[test]
    fn the_real_rc2_to_rc3_verb_mismatch_is_permanent() {
        assert_eq!(
            classify(
                "peer returned error: internal error: dispatch pod-relayed tool \
                 'pod.list': unknown tool: pod.list"
            ),
            ProbeOutcome::Permanent
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(classify("UNKNOWN TOOL: pod.list"), ProbeOutcome::Permanent);
    }

    #[test]
    fn active_refusal_is_permanent() {
        for err in [
            "Unauthorized",
            "403 Forbidden",
            "permission denied",
            "verb not allowed for this token",
            "no pinned bootstrap key for this identity",
        ] {
            assert_eq!(classify(err), ProbeOutcome::Permanent, "{err:?}");
        }
    }

    /// The guard that stops this fix becoming a worse bug. Every one of these
    /// happens routinely WHILE a peer restarts onto the new version — the exact
    /// window the gate exists to wait through. Classifying any of them as
    /// permanent would abort the gate on a perfectly healthy host.
    #[test]
    fn restart_window_errors_stay_transient() {
        for err in [
            "connect 10.0.0.9:12002: Connection refused (os error 61)",
            "connect 10.0.0.9:12002: Host is down (os error 64)",
            "No route to host (os error 65)",
            "Operation timed out (os error 60)",
            "tls handshake eof",
            "error sending request: connection closed before message completed",
            "EOF while parsing a value",
            "",
        ] {
            assert_eq!(
                classify(err),
                ProbeOutcome::Transient,
                "{err:?} must stay retryable — it happens during a normal restart"
            );
        }
    }

    #[test]
    fn classify_err_routes_through_display() {
        let e = std::io::Error::other("unknown tool: pod.list");
        assert_eq!(classify_err(&e), ProbeOutcome::Permanent);
    }
}
