//! Local store for approval requests. Never replicated: a decision is bound to
//! this node. State changes go through `contract::approval::transition` and are
//! applied with a state-guarded `UPDATE`, so two concurrent deciders cannot both
//! win.

use anyhow::{Result, anyhow};
use contract::approval::{
    ApprovalDecision, ApprovalEvent, ApprovalRequest, ApprovalState, ApprovalVerdict,
    TransitionError, transition,
};
use contract::risk::Risk;
use contract::step_up::Factor;
use rusqlite::{Connection, OptionalExtension, Row, params};

const COLUMNS: &str = "id, correlation_id, tool, action, args_hash, risk, requester_user_id,
    target_summary, plan_summary, state, created_at, expires_at, decided_by, decided_at,
    decided_factor";

fn bad(col: &str, v: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unrecognized {col} {v:?}").into(),
    )
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<ApprovalRequest> {
    let risk: String = r.get(5)?;
    let state: String = r.get(9)?;
    let factor: Option<String> = r.get(14)?;
    Ok(ApprovalRequest {
        id: r.get(0)?,
        correlation_id: r.get(1)?,
        tool: r.get(2)?,
        action: r.get(3)?,
        args_hash: r.get(4)?,
        risk: Risk::parse(&risk).ok_or_else(|| bad("risk", &risk))?,
        requester_user_id: r.get(6)?,
        target_summary: r.get(7)?,
        plan_summary: r.get(8)?,
        state: ApprovalState::parse(&state).ok_or_else(|| bad("state", &state))?,
        created_at: r.get(10)?,
        expires_at: r.get(11)?,
        decided_by: r.get(12)?,
        decided_at: r.get(13)?,
        decided_factor: factor
            .map(|f| Factor::parse(&f).ok_or_else(|| bad("factor", &f)))
            .transpose()?,
    })
}

pub fn insert(conn: &Connection, req: &ApprovalRequest) -> Result<()> {
    if !utils::id::is_uuidv7(&req.id) {
        return Err(anyhow!("approval id must be a UUIDv7, got {:?}", req.id));
    }
    // Only the `transition` state machine may produce a decided state.
    if req.state != ApprovalState::Pending
        || req.decided_by.is_some()
        || req.decided_at.is_some()
        || req.decided_factor.is_some()
    {
        return Err(anyhow!(
            "approval insert accepts only fresh pending requests"
        ));
    }
    conn.execute(
        &format!("INSERT INTO approval_requests ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"),
        params![
            req.id,
            req.correlation_id,
            req.tool,
            req.action,
            req.args_hash,
            req.risk.as_str(),
            req.requester_user_id,
            req.target_summary,
            req.plan_summary,
            req.state.as_str(),
            req.created_at,
            req.expires_at,
            req.decided_by,
            req.decided_at,
            req.decided_factor.map(Factor::as_str),
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<ApprovalRequest>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM approval_requests WHERE id = ?1"),
            params![id],
            from_row,
        )
        .optional()?)
}

/// Newest first (ids are time-ordered).
pub fn list(conn: &Connection, state: Option<ApprovalState>) -> Result<Vec<ApprovalRequest>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM approval_requests
         WHERE (?1 IS NULL OR state = ?1) ORDER BY id DESC"
    ))?;
    let rows = stmt.query_map(params![state.map(ApprovalState::as_str)], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load(conn: &Connection, id: &str) -> Result<ApprovalRequest> {
    get(conn, id)?.ok_or_else(|| anyhow!("approval request {id} not found"))
}

/// Moves `req` to `to` only if it is still in `req.state`; a concurrent change
/// loses and reports `Illegal`.
fn apply(
    conn: &Connection,
    req: &ApprovalRequest,
    to: ApprovalState,
    decided: Option<(&str, Factor, i64)>,
) -> Result<()> {
    let (by, factor, at) = match decided {
        Some((by, factor, at)) => (Some(by), Some(factor.as_str()), Some(at)),
        None => (None, None, None),
    };
    let changed = conn.execute(
        "UPDATE approval_requests
         SET state = ?1,
             decided_by = COALESCE(?2, decided_by),
             decided_factor = COALESCE(?3, decided_factor),
             decided_at = COALESCE(?4, decided_at)
         WHERE id = ?5 AND state = ?6",
        params![to.as_str(), by, factor, at, req.id, req.state.as_str()],
    )?;
    if changed == 0 {
        return Err(TransitionError::Illegal { from: req.state }.into());
    }
    Ok(())
}

/// Approve or deny. A request whose window closed is recorded as expired and
/// the approval is refused with [`TransitionError::Expired`].
pub fn decide(conn: &Connection, d: &ApprovalDecision, now_ms: i64) -> Result<ApprovalRequest> {
    let req = load(conn, &d.request_id)?;
    let event = match d.verdict {
        ApprovalVerdict::Approve => ApprovalEvent::Approve,
        ApprovalVerdict::Deny => ApprovalEvent::Deny,
    };
    match transition(&req, &event, now_ms) {
        Ok(to) => apply(conn, &req, to, Some((&d.decided_by, d.factor, now_ms)))?,
        Err(TransitionError::Expired) => {
            apply(conn, &req, ApprovalState::Expired, None)?;
            return Err(TransitionError::Expired.into());
        }
        Err(e) => return Err(e.into()),
    }
    load(conn, &d.request_id)
}

/// Consumes an approved request exactly once, bound to the args hash.
pub fn mark_executed(
    conn: &Connection,
    id: &str,
    args_hash: &str,
    now_ms: i64,
) -> Result<ApprovalRequest> {
    let req = load(conn, id)?;
    let to = transition(&req, &ApprovalEvent::Execute { args_hash }, now_ms)?;
    apply(conn, &req, to, None)?;
    load(conn, id)
}

/// Marks every pending request past its window as expired; returns the count.
pub fn expire_due(conn: &Connection, now_ms: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE approval_requests SET state = 'expired'
         WHERE state = 'pending' AND expires_at <= ?1",
        params![now_ms],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;
    use contract::approval::NewApproval;

    fn open(conn: &Connection, now: i64) -> ApprovalRequest {
        let req = ApprovalRequest::open(
            NewApproval {
                correlation_id: utils::id::new(),
                tool: "garage.bucket_delete".into(),
                action: None,
                args_hash: "h1".into(),
                risk: Risk::High,
                requester_user_id: utils::id::new(),
                target_summary: "bucket media".into(),
                plan_summary: "delete bucket media".into(),
            },
            now,
            100,
        );
        insert(conn, &req).unwrap();
        req
    }

    fn decision(id: &str, verdict: ApprovalVerdict) -> ApprovalDecision {
        ApprovalDecision {
            request_id: id.into(),
            verdict,
            decided_by: utils::id::new(),
            factor: Factor::None,
        }
    }

    fn transition_err(e: &anyhow::Error) -> TransitionError {
        *e.downcast_ref::<TransitionError>()
            .expect("transition error")
    }

    #[test]
    fn insert_get_round_trips() {
        let conn = test_conn();
        let req = open(&conn, 10);
        assert_eq!(get(&conn, &req.id).unwrap(), Some(req));
        assert_eq!(get(&conn, &utils::id::new()).unwrap(), None);
    }

    #[test]
    fn insert_rejects_a_non_v7_id() {
        let conn = test_conn();
        let mut req = open(&conn, 10);
        req.id = "f47ac10b-58cc-4372-a567-0e02b2c3d479".into();
        assert!(insert(&conn, &req).is_err());
    }

    #[test]
    fn insert_rejects_anything_but_a_fresh_pending_request() {
        let conn = test_conn();
        let base = open(&conn, 10);
        let mut forged = vec![base.clone(); 4];
        forged[0].state = ApprovalState::Approved;
        forged[1].state = ApprovalState::Executed;
        forged[2].decided_by = Some(utils::id::new());
        forged[3].decided_factor = Some(Factor::None);
        for mut req in forged {
            req.id = utils::id::new();
            assert!(insert(&conn, &req).is_err());
            assert_eq!(get(&conn, &req.id).unwrap(), None);
        }
        assert_eq!(list(&conn, None).unwrap().len(), 1);
    }

    #[test]
    fn list_filters_by_state_newest_first() {
        let conn = test_conn();
        let a = open(&conn, 10);
        let b = open(&conn, 10);
        decide(&conn, &decision(&a.id, ApprovalVerdict::Deny), 20).unwrap();
        let all: Vec<_> = list(&conn, None)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(all, [b.id.clone(), a.id.clone()]);
        let pending = list(&conn, Some(ApprovalState::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, b.id);
    }

    #[test]
    fn approve_records_the_decider_and_factor() {
        let conn = test_conn();
        let req = open(&conn, 10);
        let d = decision(&req.id, ApprovalVerdict::Approve);
        let got = decide(&conn, &d, 20).unwrap();
        assert_eq!(got.state, ApprovalState::Approved);
        assert_eq!(got.decided_by.as_deref(), Some(d.decided_by.as_str()));
        assert_eq!(
            (got.decided_at, got.decided_factor),
            (Some(20), Some(Factor::None))
        );
    }

    #[test]
    fn a_decided_request_cannot_be_decided_again() {
        let conn = test_conn();
        let req = open(&conn, 10);
        decide(&conn, &decision(&req.id, ApprovalVerdict::Approve), 20).unwrap();
        for v in [ApprovalVerdict::Approve, ApprovalVerdict::Deny] {
            let e = decide(&conn, &decision(&req.id, v), 21).unwrap_err();
            assert_eq!(
                transition_err(&e),
                TransitionError::Illegal {
                    from: ApprovalState::Approved
                }
            );
        }
    }

    #[test]
    fn approving_after_the_window_records_expiry_and_refuses() {
        let conn = test_conn();
        let req = open(&conn, 10);
        let e = decide(&conn, &decision(&req.id, ApprovalVerdict::Approve), 110).unwrap_err();
        assert_eq!(transition_err(&e), TransitionError::Expired);
        assert_eq!(
            get(&conn, &req.id).unwrap().unwrap().state,
            ApprovalState::Expired
        );
    }

    #[test]
    fn execute_is_once_only_and_bound_to_the_args_hash() {
        let conn = test_conn();
        let req = open(&conn, 10);
        decide(&conn, &decision(&req.id, ApprovalVerdict::Approve), 20).unwrap();
        let e = mark_executed(&conn, &req.id, "other", 30).unwrap_err();
        assert_eq!(transition_err(&e), TransitionError::ArgsHashMismatch);
        assert_eq!(
            mark_executed(&conn, &req.id, "h1", 30).unwrap().state,
            ApprovalState::Executed
        );
        let e = mark_executed(&conn, &req.id, "h1", 31).unwrap_err();
        assert_eq!(
            transition_err(&e),
            TransitionError::Illegal {
                from: ApprovalState::Executed
            }
        );
    }

    #[test]
    fn an_unapproved_request_cannot_execute() {
        let conn = test_conn();
        let req = open(&conn, 10);
        let e = mark_executed(&conn, &req.id, "h1", 20).unwrap_err();
        assert_eq!(
            transition_err(&e),
            TransitionError::Illegal {
                from: ApprovalState::Pending
            }
        );
    }

    #[test]
    fn expire_due_only_touches_pending_past_their_window() {
        let conn = test_conn();
        let late = open(&conn, 10);
        let fresh = open(&conn, 50);
        let approved = open(&conn, 10);
        decide(&conn, &decision(&approved.id, ApprovalVerdict::Approve), 20).unwrap();
        assert_eq!(expire_due(&conn, 110).unwrap(), 1);
        let state = |id: &str| get(&conn, id).unwrap().unwrap().state;
        assert_eq!(state(&late.id), ApprovalState::Expired);
        assert_eq!(state(&fresh.id), ApprovalState::Pending);
        assert_eq!(state(&approved.id), ApprovalState::Approved);
    }
}
