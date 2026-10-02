//! Address a resource by id; orca finds the system that owns it.
//!
//! This is the mechanism #647 slice 2 is built on. A caller names the RESOURCE
//! — a mount placement, a share, a service instance — and never a host. Where
//! the answer lives is orca's problem: this resolves the owner, answers locally
//! when the owner is this system, and otherwise asks the owner over the mesh.
//!
//! It replaces the per-domain copies of that decision. `storage.mount.detail`
//! hand-rolled it; every other owned-resource domain would have needed its own,
//! and each copy is a chance to get the local case wrong and dial ourselves, or
//! to treat an unreachable owner as an authoritative "no".
//!
//! The rule the helper encodes: **an owner that cannot be reached yields no
//! answer, never a wrong one.** A read that silently downgrades to the local
//! host's stale copy is how a non-owner reports another host's liveness as
//! fact, which is exactly what [[mount-health-read-fanout-foreign-hosts]]
//! exists to prevent.

use std::sync::Arc;

/// How long to wait on one owner before giving up on it.
///
/// A reachable owner answers a read well inside this. The bound is what keeps
/// an addressed read fast when the owner is down: it resolves to "no answer",
/// not to a hang.
pub const OWNER_READ_TIMEOUT_SECS: u64 = 5;

/// The decision itself is generic and lives in [`contract::owner`] — every
/// domain crate makes it, and `system` is not reachable from most of them.
/// Re-exported here so this module stays the one place a `system` tool looks.
pub use contract::owner::{Answers, answers_for};

/// Ask the system that owns a resource to answer for it.
///
/// `None` when there is no mesh transport on this ctx (a plugin-side call), the
/// owner is unreachable, or it answered with something undecodable. Callers
/// render that as unknown — never as the local host's own view, which would be
/// a guess presented as truth.
pub async fn ask_owner<T: contract::OrcaToolDef>(
    owner: &str,
    args: T::Args,
    ctx: &contract::ToolCtx,
) -> Option<T::Output> {
    let svc = ctx.service::<Arc<dyn contract::RemoteExec>>().ok()?;
    #[allow(clippy::disallowed_types)]
    let args = serde_json::to_value(&args).ok()?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(OWNER_READ_TIMEOUT_SECS),
        svc.exec(
            owner,
            T::NAME,
            args,
            ctx.caller(),
            ctx.correlation_id().map(str::to_string),
        ),
    )
    .await
    .ok()?
    .ok()?;
    #[allow(clippy::disallowed_types)]
    serde_json::from_value(result).ok()
}

/// How long to wait on an owner running a MUTATION before giving up on it.
///
/// Far longer than [`OWNER_READ_TIMEOUT_SECS`]: an imperative renders config,
/// reloads a daemon and waits on a mount, none of which is a 5-second read.
/// Cutting a mutation short does not undo it — it only loses the answer — so
/// the bound exists to stop an operator hanging forever, not to bound the work.
pub const OWNER_MUTATION_TIMEOUT_SECS: u64 = 300;

/// Why a mutation did not complete at its owner.
///
/// The distinction is the whole point. `ask_owner` collapses every failure to
/// `None` because a read that fails changes nothing. A mutation that fails may
/// have changed something, and an operator deciding whether to retry needs to
/// know which happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotDone {
    /// Never dispatched — nothing ran, here or anywhere. Safe to retry.
    NotAttempted(String),
    /// Dispatched to the owner; the outcome is UNKNOWN. It may have run to
    /// completion, partially, or not at all. Never retry blindly: re-read the
    /// resource first.
    OutcomeUnknown(String),
}

impl std::fmt::Display for NotDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAttempted(m) => write!(f, "not attempted, nothing ran: {m}"),
            Self::OutcomeUnknown(m) => write!(f, "dispatched but outcome unknown: {m}"),
        }
    }
}

impl std::error::Error for NotDone {}

/// Tell the system that owns a resource to perform a mutation on it.
///
/// The mutating counterpart to [`ask_owner`]. Where a failed read may fall back
/// to a local projection, a failed mutation must never fall back to acting
/// locally — that is precisely the misapplication this exists to prevent, where
/// an apply addressed at one host writes config onto whichever daemon took the
/// call. The error says whether anything was attempted; the caller propagates
/// it rather than substituting local work.
pub async fn tell_owner<T: contract::OrcaToolDef>(
    owner: &str,
    args: T::Args,
    ctx: &contract::ToolCtx,
) -> Result<T::Output, NotDone> {
    let svc = ctx
        .service::<Arc<dyn contract::RemoteExec>>()
        .map_err(|e| NotDone::NotAttempted(format!("no mesh transport on this call: {e}")))?;
    #[allow(clippy::disallowed_types)]
    let args = serde_json::to_value(&args)
        .map_err(|e| NotDone::NotAttempted(format!("could not encode args: {e}")))?;
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(OWNER_MUTATION_TIMEOUT_SECS),
        svc.exec(
            owner,
            T::NAME,
            args,
            ctx.caller(),
            ctx.correlation_id().map(str::to_string),
        ),
    )
    .await;
    let value = match sent {
        Err(_) => {
            return Err(NotDone::OutcomeUnknown(format!(
                "`{}` on `{owner}` did not answer within {OWNER_MUTATION_TIMEOUT_SECS}s; it may still be running",
                T::NAME
            )));
        }
        // A transport error cannot tell us whether the call was delivered and
        // the reply lost, or never left. Both classify as unknown: assuming
        // "never ran" is the assumption that causes a double-apply.
        Ok(Err(e)) => {
            return Err(NotDone::OutcomeUnknown(format!(
                "`{}` on `{owner}` failed in transport: {e}",
                T::NAME
            )));
        }
        Ok(Ok(v)) => v,
    };
    #[allow(clippy::disallowed_types)]
    serde_json::from_value(value).map_err(|e| {
        NotDone::OutcomeUnknown(format!(
            "`{}` on `{owner}` answered with something undecodable: {e}",
            T::NAME
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mutation_failure_says_whether_anything_ran() {
        // An operator's retry decision hangs on this wording, so assert it
        // rather than trusting the variant name to reach them.
        assert!(
            NotDone::NotAttempted("no transport".into())
                .to_string()
                .contains("nothing ran")
        );
        assert!(
            NotDone::OutcomeUnknown("timeout".into())
                .to_string()
                .contains("outcome unknown")
        );
    }

    #[test]
    fn this_systems_own_resource_is_answered_here() {
        assert_eq!(answers_for("thor", "thor"), Answers::Locally);
        // Host identity is not case-sensitive, and neither is this decision —
        // a case difference must not send a host dialing itself over the mesh.
        assert_eq!(answers_for("THOR", "thor"), Answers::Locally);
        assert_eq!(answers_for(" thor ", "thor"), Answers::Locally);
    }

    #[test]
    fn another_systems_resource_is_answered_by_that_system() {
        assert_eq!(
            answers_for("mint", "thor"),
            Answers::Owner("mint".to_string())
        );
    }

    #[test]
    fn an_unrecorded_owner_answers_here_rather_than_dialing_nowhere() {
        // Dialing "" fails with a transport error that tells an operator
        // nothing; this host speaking for an ownerless row is at least true.
        assert_eq!(answers_for("", "thor"), Answers::Locally);
        assert_eq!(answers_for("   ", "thor"), Answers::Locally);
    }
}
