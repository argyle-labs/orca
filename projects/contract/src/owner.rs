//! Who answers for a resource — the pure half of id-addressed dispatch.
//!
//! A caller names a RESOURCE and never a host (#647). Deciding which system
//! that resolves to is the same decision in every domain, so it lives here
//! rather than once per domain crate. Each copy of it is a chance to get the
//! local case wrong and have a host dial itself, or to treat an unreachable
//! owner as an authoritative "no".
//!
//! Only the decision lives here. Actually asking the owner needs a timeout and
//! therefore a runtime, which `contract` deliberately does not require of the
//! thin plugins that link it; each domain pairs this with its own dispatch.

/// Where a resource's answer must come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answers {
    /// This system owns it — answer locally. It is the termination point:
    /// orca IS the service here, and there is nothing further to reach.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_systems_own_resource_is_answered_here() {
        assert_eq!(answers_for("thor", "thor"), Answers::Locally);
        // Host identity is not case-sensitive, and neither is this decision —
        // a case difference must not send a host dialing itself over the mesh.
        assert_eq!(answers_for("THOR", "thor"), Answers::Locally);
        assert_eq!(answers_for("  thor  ", "thor"), Answers::Locally);
    }

    #[test]
    fn an_unrecorded_owner_is_answered_here_not_dialed() {
        // Dialing `""` yields a transport error that tells an operator
        // nothing. A row with no owner is this host's to speak for.
        assert_eq!(answers_for("", "thor"), Answers::Locally);
        assert_eq!(answers_for("   ", "thor"), Answers::Locally);
    }

    #[test]
    fn another_systems_resource_resolves_to_that_system() {
        assert_eq!(
            answers_for("baldur", "thor"),
            Answers::Owner("baldur".to_string())
        );
    }
}
