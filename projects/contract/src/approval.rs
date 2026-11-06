//! The approval request model and its state machine.
//!
//! [`transition`] is pure: it decides whether an event is legal for a request
//! at a given instant and returns the next state. Storage applies the result
//! atomically; nothing here touches a clock or a database.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::risk::Risk;
use crate::step_up::Factor;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    Expired,
    Executed,
}

impl ApprovalState {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalState::Pending => "pending",
            ApprovalState::Approved => "approved",
            ApprovalState::Denied => "denied",
            ApprovalState::Expired => "expired",
            ApprovalState::Executed => "executed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ApprovalState::Pending),
            "approved" => Some(ApprovalState::Approved),
            "denied" => Some(ApprovalState::Denied),
            "expired" => Some(ApprovalState::Expired),
            "executed" => Some(ApprovalState::Executed),
            _ => None,
        }
    }
}

/// Timestamps are milliseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    /// UUIDv7.
    pub id: String,
    pub correlation_id: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Hash of the exact args the approval authorises.
    pub args_hash: String,
    pub risk: Risk,
    pub requester_user_id: String,
    pub target_summary: String,
    pub plan_summary: String,
    pub state: ApprovalState,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<i64>,
    /// The factor that satisfied the decision's step-up requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_factor: Option<Factor>,
}

/// What the caller asks to open; id and state are assigned here.
#[derive(Clone, Debug)]
pub struct NewApproval {
    pub correlation_id: String,
    pub tool: String,
    pub action: Option<String>,
    pub args_hash: String,
    pub risk: Risk,
    pub requester_user_id: String,
    pub target_summary: String,
    pub plan_summary: String,
}

impl ApprovalRequest {
    pub fn open(new: NewApproval, now_ms: i64, ttl_ms: i64) -> Self {
        Self {
            id: utils::id::new(),
            correlation_id: new.correlation_id,
            tool: new.tool,
            action: new.action,
            args_hash: new.args_hash,
            risk: new.risk,
            requester_user_id: new.requester_user_id,
            target_summary: new.target_summary,
            plan_summary: new.plan_summary,
            state: ApprovalState::Pending,
            created_at: now_ms,
            expires_at: now_ms.saturating_add(ttl_ms),
            decided_by: None,
            decided_at: None,
            decided_factor: None,
        }
    }

    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalVerdict {
    Approve,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    pub request_id: String,
    pub verdict: ApprovalVerdict,
    pub decided_by: String,
    pub factor: Factor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalEvent<'a> {
    Approve,
    Deny,
    /// Records that a pending request ran out of time.
    Expire,
    /// Runs the approved action with these args' hash.
    Execute {
        args_hash: &'a str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionError {
    /// The event is not legal from the current state.
    Illegal {
        from: ApprovalState,
    },
    /// The request's window closed before the event; the request is expired.
    Expired,
    /// `Expire` arrived before the window closed.
    NotYetExpired,
    ArgsHashMismatch,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::Illegal { from } => {
                write!(f, "illegal approval transition from {}", from.as_str())
            }
            TransitionError::Expired => f.write_str("approval request expired"),
            TransitionError::NotYetExpired => f.write_str("approval request has not expired"),
            TransitionError::ArgsHashMismatch => {
                f.write_str("args differ from the approved request")
            }
        }
    }
}

impl std::error::Error for TransitionError {}

pub fn transition(
    req: &ApprovalRequest,
    event: &ApprovalEvent<'_>,
    now_ms: i64,
) -> Result<ApprovalState, TransitionError> {
    use ApprovalState as S;
    let expired = req.is_expired_at(now_ms);
    match (req.state, event) {
        (S::Pending, ApprovalEvent::Approve) if expired => Err(TransitionError::Expired),
        (S::Pending, ApprovalEvent::Approve) => Ok(S::Approved),
        (S::Pending, ApprovalEvent::Deny) => Ok(S::Denied),
        (S::Pending, ApprovalEvent::Expire) if expired => Ok(S::Expired),
        (S::Pending, ApprovalEvent::Expire) => Err(TransitionError::NotYetExpired),
        (S::Approved, ApprovalEvent::Execute { .. }) if expired => Err(TransitionError::Expired),
        (S::Approved, ApprovalEvent::Execute { args_hash }) => {
            if *args_hash == req.args_hash {
                Ok(S::Executed)
            } else {
                Err(TransitionError::ArgsHashMismatch)
            }
        }
        (from, _) => Err(TransitionError::Illegal { from }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [ApprovalState; 5] = [
        ApprovalState::Pending,
        ApprovalState::Approved,
        ApprovalState::Denied,
        ApprovalState::Expired,
        ApprovalState::Executed,
    ];

    fn req(state: ApprovalState) -> ApprovalRequest {
        let mut r = ApprovalRequest::open(
            NewApproval {
                correlation_id: utils::id::new(),
                tool: "garage.bucket_delete".into(),
                action: Some("delete".into()),
                args_hash: "h1".into(),
                risk: Risk::High,
                requester_user_id: utils::id::new(),
                target_summary: "bucket media".into(),
                plan_summary: "delete bucket media".into(),
            },
            1_000,
            500,
        );
        r.state = state;
        r
    }

    const BEFORE: i64 = 1_499;
    const AT_EXPIRY: i64 = 1_500;

    #[test]
    fn open_mints_a_uuidv7_pending_request() {
        let r = req(ApprovalState::Pending);
        assert!(utils::id::is_uuidv7(&r.id));
        assert_eq!((r.state, r.expires_at), (ApprovalState::Pending, 1_500));
    }

    #[test]
    fn full_table_of_states_and_events_before_expiry() {
        use ApprovalState as S;
        let events = [
            ApprovalEvent::Approve,
            ApprovalEvent::Deny,
            ApprovalEvent::Expire,
            ApprovalEvent::Execute { args_hash: "h1" },
        ];
        let expect = |s: S, e: &ApprovalEvent<'_>| -> Result<S, TransitionError> {
            match (s, e) {
                (S::Pending, ApprovalEvent::Approve) => Ok(S::Approved),
                (S::Pending, ApprovalEvent::Deny) => Ok(S::Denied),
                (S::Pending, ApprovalEvent::Expire) => Err(TransitionError::NotYetExpired),
                (S::Approved, ApprovalEvent::Execute { .. }) => Ok(S::Executed),
                (from, _) => Err(TransitionError::Illegal { from }),
            }
        };
        for s in STATES {
            for e in &events {
                assert_eq!(transition(&req(s), e, BEFORE), expect(s, e), "{s:?} {e:?}");
            }
        }
    }

    #[test]
    fn expired_pending_cannot_be_approved_but_can_be_denied_or_expired() {
        let r = req(ApprovalState::Pending);
        assert_eq!(
            transition(&r, &ApprovalEvent::Approve, AT_EXPIRY),
            Err(TransitionError::Expired)
        );
        assert_eq!(
            transition(&r, &ApprovalEvent::Expire, AT_EXPIRY),
            Ok(ApprovalState::Expired)
        );
        assert_eq!(
            transition(&r, &ApprovalEvent::Deny, AT_EXPIRY),
            Ok(ApprovalState::Denied)
        );
    }

    #[test]
    fn approved_request_cannot_execute_after_expiry() {
        let r = req(ApprovalState::Approved);
        assert_eq!(
            transition(&r, &ApprovalEvent::Execute { args_hash: "h1" }, AT_EXPIRY),
            Err(TransitionError::Expired)
        );
    }

    #[test]
    fn changed_args_refuse_execution() {
        let r = req(ApprovalState::Approved);
        assert_eq!(
            transition(&r, &ApprovalEvent::Execute { args_hash: "h2" }, BEFORE),
            Err(TransitionError::ArgsHashMismatch)
        );
    }

    #[test]
    fn executes_exactly_once_and_cannot_be_reapproved() {
        let mut r = req(ApprovalState::Approved);
        r.state = transition(&r, &ApprovalEvent::Execute { args_hash: "h1" }, BEFORE).unwrap();
        assert_eq!(r.state, ApprovalState::Executed);
        for e in [
            ApprovalEvent::Execute { args_hash: "h1" },
            ApprovalEvent::Approve,
        ] {
            assert_eq!(
                transition(&r, &e, BEFORE),
                Err(TransitionError::Illegal {
                    from: ApprovalState::Executed
                })
            );
        }
    }

    #[test]
    fn terminal_states_accept_nothing_even_after_expiry() {
        for s in [
            ApprovalState::Denied,
            ApprovalState::Expired,
            ApprovalState::Executed,
        ] {
            for e in [
                ApprovalEvent::Approve,
                ApprovalEvent::Deny,
                ApprovalEvent::Expire,
                ApprovalEvent::Execute { args_hash: "h1" },
            ] {
                assert_eq!(
                    transition(&req(s), &e, AT_EXPIRY),
                    Err(TransitionError::Illegal { from: s })
                );
            }
        }
    }

    #[test]
    fn state_strings_round_trip() {
        for s in STATES {
            assert_eq!(ApprovalState::parse(s.as_str()), Some(s));
        }
        assert_eq!(ApprovalState::parse("done"), None);
    }
}
