//! Per-host capability registry — see
//! `migrations/20260615200000__host_capabilities.up.sql`.
//!
//! Written by `system::capability::probe_all_capabilities` at daemon
//! startup and by `system.capability.{recheck,disable,enable}` tools.
//! Read by collectors and tool surfaces (topology, containers, vms) so
//! absent providers stay silent — no warn-every-tick when docker isn't
//! installed.
//!
//! `Disabled` is sticky across daemon restarts. `Available` and `Absent`
//! are derived fresh from probes on each startup.

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityState {
    Available,
    Absent,
    Disabled,
}

impl CapabilityState {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityState::Available => "available",
            CapabilityState::Absent => "absent",
            CapabilityState::Disabled => "disabled",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "available" => Ok(CapabilityState::Available),
            "absent" => Ok(CapabilityState::Absent),
            "disabled" => Ok(CapabilityState::Disabled),
            other => Err(anyhow!("unknown capability state `{other}`")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostCapability {
    pub provider: String,
    pub state: CapabilityState,
    /// Unix epoch seconds; advanced on every probe (and on disable/enable).
    pub last_probed: i64,
    /// Human-readable failure/disable reason. None when Available.
    pub reason: Option<String>,
    /// Version string when Available (e.g. `"Docker 27.0.3"`). None otherwise.
    pub detail: Option<String>,
}

pub fn upsert(conn: &Connection, row: &HostCapability) -> Result<()> {
    conn.execute(
        "INSERT INTO host_capabilities (provider, state, last_probed, reason, detail)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider) DO UPDATE SET
            state       = excluded.state,
            last_probed = excluded.last_probed,
            reason      = excluded.reason,
            detail      = excluded.detail",
        params![
            row.provider,
            row.state.as_str(),
            row.last_probed,
            row.reason,
            row.detail,
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, provider: &str) -> Result<Option<HostCapability>> {
    let row = conn
        .query_row(
            "SELECT provider, state, last_probed, reason, detail
             FROM host_capabilities WHERE provider = ?1",
            params![provider],
            row_from,
        )
        .optional()?;
    Ok(row)
}

pub fn list_all(conn: &Connection) -> Result<Vec<HostCapability>> {
    let mut stmt = conn.prepare(
        "SELECT provider, state, last_probed, reason, detail
         FROM host_capabilities ORDER BY provider",
    )?;
    let rows = stmt
        .query_map([], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete(conn: &Connection, provider: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM host_capabilities WHERE provider = ?1",
        params![provider],
    )?;
    Ok(n > 0)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<HostCapability> {
    let state_s: String = r.get(1)?;
    let state = CapabilityState::parse(&state_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(e.to_string())),
        )
    })?;
    Ok(HostCapability {
        provider: r.get(0)?,
        state,
        last_probed: r.get(2)?,
        reason: r.get(3)?,
        detail: r.get(4)?,
    })
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

    #[test]
    fn upsert_then_get_roundtrips() {
        let c = mem();
        let row = HostCapability {
            provider: "docker".into(),
            state: CapabilityState::Available,
            last_probed: 1_700_000_000,
            reason: None,
            detail: Some("Docker 27.0.3".into()),
        };
        upsert(&c, &row).unwrap();
        let got = get(&c, "docker").unwrap().expect("present");
        assert_eq!(got.state, CapabilityState::Available);
        assert_eq!(got.detail.as_deref(), Some("Docker 27.0.3"));
        assert!(got.reason.is_none());
    }

    #[test]
    fn upsert_overwrites_state_and_detail() {
        let c = mem();
        upsert(
            &c,
            &HostCapability {
                provider: "docker".into(),
                state: CapabilityState::Available,
                last_probed: 1,
                reason: None,
                detail: Some("v1".into()),
            },
        )
        .unwrap();
        upsert(
            &c,
            &HostCapability {
                provider: "docker".into(),
                state: CapabilityState::Absent,
                last_probed: 2,
                reason: Some("binary not in PATH".into()),
                detail: None,
            },
        )
        .unwrap();
        let got = get(&c, "docker").unwrap().unwrap();
        assert_eq!(got.state, CapabilityState::Absent);
        assert_eq!(got.reason.as_deref(), Some("binary not in PATH"));
        assert!(got.detail.is_none());
    }

    #[test]
    fn list_all_returns_sorted() {
        let c = mem();
        for p in ["proxmox", "docker", "unraid"] {
            upsert(
                &c,
                &HostCapability {
                    provider: p.into(),
                    state: CapabilityState::Available,
                    last_probed: 1,
                    reason: None,
                    detail: None,
                },
            )
            .unwrap();
        }
        let rows = list_all(&c).unwrap();
        let names: Vec<_> = rows.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(names, vec!["docker", "proxmox", "unraid"]);
    }

    #[test]
    fn delete_removes_row() {
        let c = mem();
        upsert(
            &c,
            &HostCapability {
                provider: "docker".into(),
                state: CapabilityState::Disabled,
                last_probed: 1,
                reason: Some("op".into()),
                detail: None,
            },
        )
        .unwrap();
        assert!(delete(&c, "docker").unwrap());
        assert!(get(&c, "docker").unwrap().is_none());
        assert!(!delete(&c, "docker").unwrap());
    }

    #[test]
    fn parse_round_trips() {
        for s in ["available", "absent", "disabled"] {
            assert_eq!(CapabilityState::parse(s).unwrap().as_str(), s);
        }
        assert!(CapabilityState::parse("wat").is_err());
    }
}
