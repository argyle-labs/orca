//! Dismissable (stateful) notifications store.
//!
//! The second notification plane. The first — `notifications::emit(Event)` —
//! is EPHEMERAL: it fans an event out to backends and keeps no state. This is
//! the STATEFUL plane: notifications that persist with a lifecycle
//! (`active` → `dismissed`/`suppressed`), are ingested from many sources
//! (external systems + orca's own diagnostics), are classified by *audience*
//! (user vs system), and can be dismissed locally (and later at their source).
//!
//! `key` is a stable dedup id (e.g. `unraid:<host>:<src_id>`,
//! `diag:<provider>:<finding_id>`). Re-[`raise`]ing the same key is an
//! idempotent upsert that reactivates the row and refreshes its fields;
//! raising a `suppressed` key is a **no-op** — that is what "ignore
//! permanently" means. Schema lives in
//! `migrations/20260714000000__dismissable_notifications.up.sql`.
//!
//! This crate deliberately does not depend on the `notifications` crate, so
//! [`Severity`] here is local; the diagnostics→notification bridge (a later
//! change) maps between `diagnostics::Severity`, `notifications::Severity`,
//! and this one. All timestamps are unix milliseconds.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Severity ladder. Ordered so `>= Error` is meaningful for audience routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
            Severity::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "info" => Severity::Info,
            "warn" | "warning" => Severity::Warn,
            "error" => Severity::Error,
            "critical" | "crit" | "alert" => Severity::Critical,
            other => bail!("unknown severity `{other}`"),
        })
    }
}

/// Who a notification is for. Derived on raise; see [`derive_audience`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// Surfaced to the user (pushed once through the ephemeral dispatcher too).
    User,
    /// Recorded and queryable by orca, but not pushed to the user.
    System,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Audience::User => "user",
            Audience::System => "system",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "user" => Audience::User,
            "system" => Audience::System,
            other => bail!("unknown audience `{other}`"),
        })
    }
}

/// Lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Live and (if user-audience) shown to the user.
    Active,
    /// User dismissed it. A later re-raise reactivates it.
    Dismissed,
    /// "Ignore permanently" — a later re-raise is a no-op.
    Suppressed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Dismissed => "dismissed",
            State::Suppressed => "suppressed",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "active" => State::Active,
            "dismissed" => State::Dismissed,
            "suppressed" => State::Suppressed,
            other => bail!("unknown state `{other}`"),
        })
    }
}

/// Optional remediation attached to a notification. An external URL and/or an
/// in-orca deep link (a diagnostics repair, or a unit action). All fields
/// optional; an all-`None` `Fix` is treated as "no fix".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fix {
    /// External page that documents or performs the fix.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub url: Option<String>,
    /// Diagnostics provider that owns the repair (pairs with `repair_id`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider: Option<String>,
    /// `RepairSpec` id to invoke via `diagnostics.repair`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repair_id: Option<String>,
    /// Canonical unit id the fix acts on (pairs with `action`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unit: Option<String>,
    /// Action verb to run against `unit`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub action: Option<String>,
}

impl Fix {
    fn is_empty(&self) -> bool {
        self.url.is_none()
            && self.provider.is_none()
            && self.repair_id.is_none()
            && self.unit.is_none()
            && self.action.is_none()
    }
}

/// A persisted dismissable notification row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub key: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub severity: Severity,
    pub actionable: bool,
    pub fix: Option<Fix>,
    pub title: String,
    pub body: Option<String>,
    pub audience: Audience,
    pub state: State,
    pub user_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Everything a caller supplies to [`raise`]. `audience` is derived, `state`
/// and timestamps are managed by the store, so they are absent here.
#[derive(Debug, Clone)]
pub struct RaiseInput {
    pub key: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub severity: Severity,
    pub actionable: bool,
    pub fix: Option<Fix>,
    pub title: String,
    pub body: Option<String>,
    pub user_id: Option<String>,
}

/// Audience policy: surface to the USER iff the severity is `Error`+ OR the
/// notification is actionable. Everything else stays `System` — recorded and
/// queryable, but not pushed to the user. (Driver: "we receive ALL unraid
/// notifications; the user only wants actionable + errors; non-actionable
/// warnings stay system-side.")
pub fn derive_audience(severity: Severity, actionable: bool) -> Audience {
    if actionable || severity >= Severity::Error {
        Audience::User
    } else {
        Audience::System
    }
}

/// Filter for [`list`]. `None` fields match everything.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub state: Option<State>,
    pub audience: Option<Audience>,
}

/// Raise (create or reactivate) a notification.
///
/// * A `suppressed` row with this key is left untouched — raising it is a no-op
///   and the existing (suppressed) row is returned.
/// * An existing `active`/`dismissed` row is upserted: its fields are refreshed
///   and its state is set back to `active` (a recurring condition re-surfaces),
///   preserving `created_at`.
/// * Otherwise a fresh `active` row is inserted.
///
/// `audience` is always (re)derived from `severity`+`actionable`.
pub fn raise(conn: &Connection, input: RaiseInput, now_ms: i64) -> Result<Notification> {
    if let Some(existing) = get(conn, &input.key)?
        && existing.state == State::Suppressed
    {
        return Ok(existing);
    }

    let audience = derive_audience(input.severity, input.actionable);
    let fix_json = match &input.fix {
        Some(f) if !f.is_empty() => Some(serde_json::to_string(f)?),
        _ => None,
    };

    conn.execute(
        "INSERT INTO dismissable_notifications
            (key, source, source_ref, severity, actionable, fix, title, body,
             audience, state, user_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', ?10, ?11, ?11)
         ON CONFLICT(key) DO UPDATE SET
            source     = excluded.source,
            source_ref = excluded.source_ref,
            severity   = excluded.severity,
            actionable = excluded.actionable,
            fix        = excluded.fix,
            title      = excluded.title,
            body       = excluded.body,
            audience   = excluded.audience,
            state      = 'active',
            user_id    = excluded.user_id,
            updated_at = excluded.updated_at",
        params![
            input.key,
            input.source,
            input.source_ref,
            input.severity.as_str(),
            input.actionable as i64,
            fix_json,
            input.title,
            input.body,
            audience.as_str(),
            input.user_id,
            now_ms,
        ],
    )?;

    get(conn, &input.key)?.ok_or_else(|| anyhow::anyhow!("notification vanished after raise"))
}

/// Transition a notification to a terminal-ish state. Returns the updated row,
/// or `None` if no row with `key` exists. Idempotent.
fn set_state(
    conn: &Connection,
    key: &str,
    state: State,
    now_ms: i64,
) -> Result<Option<Notification>> {
    conn.execute(
        "UPDATE dismissable_notifications SET state = ?2, updated_at = ?3 WHERE key = ?1",
        params![key, state.as_str(), now_ms],
    )?;
    get(conn, key)
}

/// Dismiss a notification (user acknowledged it). A later re-raise reactivates.
pub fn dismiss(conn: &Connection, key: &str, now_ms: i64) -> Result<Option<Notification>> {
    set_state(conn, key, State::Dismissed, now_ms)
}

/// Suppress a notification permanently ("ignore permanently"). A later
/// re-raise is a no-op until this row is deleted.
pub fn suppress(conn: &Connection, key: &str, now_ms: i64) -> Result<Option<Notification>> {
    set_state(conn, key, State::Suppressed, now_ms)
}

pub fn get(conn: &Connection, key: &str) -> Result<Option<Notification>> {
    let row = conn
        .query_row(
            "SELECT key, source, source_ref, severity, actionable, fix, title, body,
                    audience, state, user_id, created_at, updated_at
             FROM dismissable_notifications WHERE key = ?1",
            params![key],
            row_from,
        )
        .optional()?;
    row.transpose()
}

/// List notifications, newest first. Filters are ANDed; `None` matches all.
pub fn list(conn: &Connection, filter: &ListFilter) -> Result<Vec<Notification>> {
    let mut sql = String::from(
        "SELECT key, source, source_ref, severity, actionable, fix, title, body,
                audience, state, user_id, created_at, updated_at
         FROM dismissable_notifications",
    );
    let mut clauses: Vec<&str> = Vec::new();
    if filter.state.is_some() {
        clauses.push("state = :state");
    }
    if filter.audience.is_some() {
        clauses.push("audience = :audience");
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY updated_at DESC, key ASC");

    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
    let state_str = filter.state.map(State::as_str);
    let audience_str = filter.audience.map(Audience::as_str);
    if let Some(s) = &state_str {
        params.push((":state", s));
    }
    if let Some(a) = &audience_str {
        params.push((":audience", a));
    }
    let rows = stmt
        .query_map(params.as_slice(), row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().collect()
}

/// rusqlite row → `Result<Notification>` (the outer `Result` is for the JSON /
/// enum parses that can fail on a corrupt row; the inner `rusqlite::Result`
/// is for column access).
fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Notification>> {
    let severity_s: String = r.get(3)?;
    let audience_s: String = r.get(8)?;
    let state_s: String = r.get(9)?;
    let fix_s: Option<String> = r.get(5)?;
    let actionable_i: i64 = r.get(4)?;

    Ok((|| {
        Ok(Notification {
            key: r.get(0)?,
            source: r.get(1)?,
            source_ref: r.get(2)?,
            severity: Severity::parse(&severity_s)?,
            actionable: actionable_i != 0,
            fix: match fix_s {
                Some(s) => Some(serde_json::from_str(&s)?),
                None => None,
            },
            title: r.get(6)?,
            body: r.get(7)?,
            audience: Audience::parse(&audience_s)?,
            state: State::parse(&state_s)?,
            user_id: r.get(10)?,
            created_at: r.get(11)?,
            updated_at: r.get(12)?,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::apply_schema(&c).unwrap();
        crate::run_pending_migrations(&c).unwrap();
        c
    }

    fn input(key: &str, sev: Severity, actionable: bool) -> RaiseInput {
        RaiseInput {
            key: key.into(),
            source: "test".into(),
            source_ref: None,
            severity: sev,
            actionable,
            fix: None,
            title: "t".into(),
            body: None,
            user_id: None,
        }
    }

    #[test]
    fn audience_policy_user_on_error_or_actionable() {
        assert_eq!(derive_audience(Severity::Info, false), Audience::System);
        assert_eq!(derive_audience(Severity::Warn, false), Audience::System);
        // Non-actionable warning stays system-side (the unraid driver case).
        assert_eq!(derive_audience(Severity::Warn, false), Audience::System);
        // Actionable warning is promoted to the user.
        assert_eq!(derive_audience(Severity::Warn, true), Audience::User);
        assert_eq!(derive_audience(Severity::Error, false), Audience::User);
        assert_eq!(derive_audience(Severity::Critical, false), Audience::User);
    }

    #[test]
    fn raise_then_get_roundtrips_and_derives_audience() {
        let c = mem();
        let n = raise(&c, input("k1", Severity::Warn, false), 1000).unwrap();
        assert_eq!(n.audience, Audience::System);
        assert_eq!(n.state, State::Active);
        assert_eq!(n.created_at, 1000);
        assert_eq!(n.updated_at, 1000);
        let got = get(&c, "k1").unwrap().unwrap();
        assert_eq!(got, n);
    }

    #[test]
    fn raise_is_idempotent_upsert_preserving_created_at() {
        let c = mem();
        raise(&c, input("k", Severity::Info, false), 100).unwrap();
        let mut second = input("k", Severity::Error, false);
        second.title = "updated".into();
        let n = raise(&c, second, 200).unwrap();
        assert_eq!(n.created_at, 100, "created_at preserved across re-raise");
        assert_eq!(n.updated_at, 200);
        assert_eq!(n.title, "updated");
        assert_eq!(
            n.audience,
            Audience::User,
            "severity bump re-derives audience"
        );
        assert_eq!(list(&c, &ListFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn dismiss_then_reraise_reactivates() {
        let c = mem();
        raise(&c, input("k", Severity::Error, false), 1).unwrap();
        let d = dismiss(&c, "k", 2).unwrap().unwrap();
        assert_eq!(d.state, State::Dismissed);
        let r = raise(&c, input("k", Severity::Error, false), 3).unwrap();
        assert_eq!(
            r.state,
            State::Active,
            "re-raise reactivates a dismissed row"
        );
    }

    #[test]
    fn suppress_makes_reraise_a_noop() {
        let c = mem();
        raise(&c, input("k", Severity::Error, false), 1).unwrap();
        suppress(&c, "k", 2).unwrap();
        let mut louder = input("k", Severity::Critical, true);
        louder.title = "should not apply".into();
        let n = raise(&c, louder, 3).unwrap();
        assert_eq!(n.state, State::Suppressed, "suppressed stays suppressed");
        assert_eq!(n.title, "t", "suppressed row is not overwritten");
    }

    #[test]
    fn fix_roundtrips_through_json() {
        let c = mem();
        let mut i = input("k", Severity::Error, true);
        i.fix = Some(Fix {
            provider: Some("proxmox".into()),
            repair_id: Some("install-qemu-guest-agent".into()),
            url: Some("https://example.invalid/docs".into()),
            ..Default::default()
        });
        raise(&c, i, 1).unwrap();
        let got = get(&c, "k").unwrap().unwrap();
        let fix = got.fix.expect("fix present");
        assert_eq!(fix.provider.as_deref(), Some("proxmox"));
        assert_eq!(fix.repair_id.as_deref(), Some("install-qemu-guest-agent"));
        assert!(fix.unit.is_none());
    }

    #[test]
    fn empty_fix_is_stored_as_none() {
        let c = mem();
        let mut i = input("k", Severity::Info, false);
        i.fix = Some(Fix::default());
        raise(&c, i, 1).unwrap();
        assert!(get(&c, "k").unwrap().unwrap().fix.is_none());
    }

    #[test]
    fn list_filters_by_state_and_audience_newest_first() {
        let c = mem();
        raise(&c, input("sys", Severity::Info, false), 10).unwrap(); // system
        raise(&c, input("usr", Severity::Error, false), 20).unwrap(); // user
        dismiss(&c, "usr", 30).unwrap();

        let active = list(
            &c,
            &ListFilter {
                state: Some(State::Active),
                audience: None,
            },
        )
        .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key, "sys");

        let user = list(
            &c,
            &ListFilter {
                state: None,
                audience: Some(Audience::User),
            },
        )
        .unwrap();
        assert_eq!(user.len(), 1);
        assert_eq!(user[0].key, "usr");

        let all = list(&c, &ListFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].key, "usr", "newest (updated_at desc) first");
    }

    #[test]
    fn dismiss_missing_key_is_none() {
        let c = mem();
        assert!(dismiss(&c, "nope", 1).unwrap().is_none());
    }
}
