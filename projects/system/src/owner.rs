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

/// Where a resource's answer must come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answers {
    /// This system owns it — answer locally. It is the termination point.
    Locally,
    /// Another system owns it; ask that one.
    Owner(String),
}

/// Decide who answers for a resource owned by `owner`, given `this_host`.
///
/// Pure, so the decision is testable without a mesh. An empty or unknown owner
/// answers locally rather than dialing nowhere — a row with no recorded owner
/// is this host's to speak for, and dialing `""` would fail with a transport
/// error that says nothing useful.
pub fn answers_for(owner: &str, this_host: &str) -> Answers {
    let owner = owner.trim();
    if owner.is_empty() || owner.eq_ignore_ascii_case(this_host.trim()) {
        Answers::Locally
    } else {
        Answers::Owner(owner.to_string())
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
