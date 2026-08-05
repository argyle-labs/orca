//! Encrypted time-series store — a SEPARATE SQLCipher database
//! (`~/.orca/metrics.db`) that never pollutes `orca.db`.
//!
//! This is the generic backing store for the history subsystem: any domain
//! records named series (`system`, later `containers`, `network`, …) and reads
//! cursor-paginated windows. Metrics are bulk time-series — they do NOT belong
//! in the relational `orca.db` — but they must still be encrypted at rest, so
//! this reuses orca's SQLCipher cipher pragmas + shared `.db_key` via
//! [`crate::open_encrypted`] (zero new dependencies, one key for every orca DB).
//!
//! Storage is generic: `samples(series, ts_ms, payload)` where `payload` is the
//! domain's own JSON sample shape. The subsystem is type-agnostic; each domain
//! owns the (de)serialization of its payload. Walkable = keyset pagination on
//! `(series, ts_ms)`.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

/// One stored sample: a timestamp (unix ms) + the domain's JSON payload.
#[derive(Debug, Clone)]
pub struct Sample {
    pub ts_ms: i64,
    pub payload: String,
}

/// A cursor-paginated window of samples, newest first.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub samples: Vec<Sample>,
    /// Opaque cursor (a `ts_ms` boundary) to pass back as `cursor` for the next
    /// older page. `None` when the window is exhausted.
    pub next_cursor: Option<i64>,
}

fn metrics_path() -> Result<PathBuf> {
    Ok(contract::config::state_dir()?.join("metrics.db"))
}

// Single shared connection guarded by a mutex. The write rate is low (one
// sample per refresher tick per series) and reads are infrequent, so a pool is
// unnecessary; WAL lets a reader proceed while a write holds the lock.
static CONN: Mutex<Option<Connection>> = Mutex::new(None);

/// Create the metrics schema. Idempotent. Called once per process on first use
/// and directly by tests against a throwaway connection.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS samples (
            series  TEXT    NOT NULL,
            ts_ms   INTEGER NOT NULL,
            payload TEXT    NOT NULL,
            PRIMARY KEY (series, ts_ms)
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS retention (
            series     TEXT PRIMARY KEY,
            max_age_ms INTEGER
        );
        ",
    )
    .context("failed to init metrics.db schema")
}

/// Run `f` with the process-shared metrics connection, opening + initializing it
/// on first use. Serialized across the process.
fn with_conn<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let mut guard = CONN.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        let conn = crate::open_encrypted(&metrics_path()?)?;
        init_schema(&conn)?;
        *guard = Some(conn);
    }
    f(guard.as_ref().expect("metrics conn initialized above"))
}

// ── core operations (take &Connection so tests can drive a throwaway DB) ──────

/// Append one sample. Upserts on `(series, ts_ms)` so a re-emitted tick replaces
/// rather than duplicates.
pub fn record_on(conn: &Connection, series: &str, ts_ms: i64, payload: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO samples (series, ts_ms, payload) VALUES (?1, ?2, ?3)
         ON CONFLICT(series, ts_ms) DO UPDATE SET payload = excluded.payload",
        rusqlite::params![series, ts_ms, payload],
    )
    .context("metrics record")?;
    Ok(())
}

/// Read a newest-first, cursor-paginated window of a series. `end_ms` (inclusive
/// upper bound, unix ms) starts the window — pass a prior page's `next_cursor`
/// to walk older. `start_ms` is an inclusive lower bound (0 = beginning of
/// time). `limit` is clamped to `[1, 5000]`.
pub fn query_on(
    conn: &Connection,
    series: &str,
    start_ms: i64,
    end_ms: i64,
    limit: usize,
) -> Result<Page> {
    let limit = limit.clamp(1, 5000);
    let mut stmt = conn.prepare(
        "SELECT ts_ms, payload FROM samples
         WHERE series = ?1 AND ts_ms >= ?2 AND ts_ms <= ?3
         ORDER BY ts_ms DESC
         LIMIT ?4",
    )?;
    // Fetch one extra row to derive the next cursor without a second query.
    let fetch = (limit + 1) as i64;
    let rows = stmt
        .query_map(rusqlite::params![series, start_ms, end_ms, fetch], |r| {
            Ok(Sample {
                ts_ms: r.get(0)?,
                payload: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut samples = rows;
    let next_cursor = if samples.len() > limit {
        // The extra row belongs to the next page; its predecessor's ts − 1 is
        // the inclusive upper bound for the following window.
        let boundary = samples[limit].ts_ms;
        samples.truncate(limit);
        Some(boundary)
    } else {
        None
    };
    Ok(Page {
        samples,
        next_cursor,
    })
}

/// Delete samples of `series` older than `cutoff_ms`. Returns rows removed.
pub fn sweep_on(conn: &Connection, series: &str, cutoff_ms: i64) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM samples WHERE series = ?1 AND ts_ms < ?2",
        rusqlite::params![series, cutoff_ms],
    )?;
    Ok(n)
}

/// Per-series retention override (max age in ms). `None` = inherit the system
/// default. Setting `None` clears the override.
pub fn set_retention_on(conn: &Connection, series: &str, max_age_ms: Option<i64>) -> Result<()> {
    match max_age_ms {
        Some(ms) => conn.execute(
            "INSERT INTO retention (series, max_age_ms) VALUES (?1, ?2)
             ON CONFLICT(series) DO UPDATE SET max_age_ms = excluded.max_age_ms",
            rusqlite::params![series, ms],
        )?,
        None => conn.execute(
            "DELETE FROM retention WHERE series = ?1",
            rusqlite::params![series],
        )?,
    };
    Ok(())
}

/// The configured retention (max age, ms) for `series`, or `None` when it
/// inherits the system default.
pub fn retention_on(conn: &Connection, series: &str) -> Result<Option<i64>> {
    let v = conn
        .query_row(
            "SELECT max_age_ms FROM retention WHERE series = ?1",
            rusqlite::params![series],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .context("metrics retention lookup")?
        .flatten();
    Ok(v)
}

// ── process-shared wrappers (open metrics.db lazily) ──────────────────────────

/// Record `payload` (already-serialized JSON) for `series` at `ts_ms`.
pub fn record(series: &str, ts_ms: i64, payload: &str) -> Result<()> {
    with_conn(|c| record_on(c, series, ts_ms, payload))
}

/// Serialize `value` to JSON and record it. Convenience for domains that record
/// a typed sample shape.
pub fn record_json<T: serde::Serialize>(series: &str, ts_ms: i64, value: &T) -> Result<()> {
    let payload = serde_json::to_string(value).context("serialize metrics sample")?;
    record(series, ts_ms, &payload)
}

/// Newest-first cursor-paginated window over the shared metrics.db.
pub fn query(series: &str, start_ms: i64, end_ms: i64, limit: usize) -> Result<Page> {
    with_conn(|c| query_on(c, series, start_ms, end_ms, limit))
}

/// Delete samples of `series` older than `cutoff_ms`. Returns rows removed.
pub fn sweep(series: &str, cutoff_ms: i64) -> Result<usize> {
    with_conn(|c| sweep_on(c, series, cutoff_ms))
}

/// Set (or clear with `None`) the per-series retention override.
pub fn set_retention(series: &str, max_age_ms: Option<i64>) -> Result<()> {
    with_conn(|c| set_retention_on(c, series, max_age_ms))
}

/// The per-series retention override, or `None` when inheriting the default.
pub fn retention(series: &str) -> Result<Option<i64>> {
    with_conn(|c| retention_on(c, series))
}

use rusqlite::OptionalExtension as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn record_and_query_newest_first() {
        let c = mem();
        for ts in [100, 200, 300, 400] {
            record_on(&c, "system", ts, &format!("{{\"v\":{ts}}}")).unwrap();
        }
        let page = query_on(&c, "system", 0, i64::MAX, 10).unwrap();
        let got: Vec<i64> = page.samples.iter().map(|s| s.ts_ms).collect();
        assert_eq!(got, vec![400, 300, 200, 100]);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn upsert_replaces_same_ts() {
        let c = mem();
        record_on(&c, "system", 100, "\"a\"").unwrap();
        record_on(&c, "system", 100, "\"b\"").unwrap();
        let page = query_on(&c, "system", 0, i64::MAX, 10).unwrap();
        assert_eq!(page.samples.len(), 1);
        assert_eq!(page.samples[0].payload, "\"b\"");
    }

    #[test]
    fn cursor_pages_older() {
        let c = mem();
        for ts in 1..=5 {
            record_on(&c, "system", ts * 100, "\"x\"").unwrap();
        }
        // First page: newest 2 (500, 400), cursor points below 400.
        let p1 = query_on(&c, "system", 0, i64::MAX, 2).unwrap();
        assert_eq!(
            p1.samples.iter().map(|s| s.ts_ms).collect::<Vec<_>>(),
            vec![500, 400]
        );
        let cursor = p1.next_cursor.expect("more pages");
        assert_eq!(cursor, 300);
        // Next page ends at the cursor: 300, 200.
        let p2 = query_on(&c, "system", 0, cursor, 2).unwrap();
        assert_eq!(
            p2.samples.iter().map(|s| s.ts_ms).collect::<Vec<_>>(),
            vec![300, 200]
        );
    }

    #[test]
    fn series_are_isolated() {
        let c = mem();
        record_on(&c, "system", 100, "\"s\"").unwrap();
        record_on(&c, "containers", 100, "\"c\"").unwrap();
        assert_eq!(
            query_on(&c, "system", 0, i64::MAX, 10)
                .unwrap()
                .samples
                .len(),
            1
        );
        assert_eq!(
            query_on(&c, "containers", 0, i64::MAX, 10).unwrap().samples[0].payload,
            "\"c\""
        );
    }

    #[test]
    fn sweep_drops_old() {
        let c = mem();
        for ts in [100, 200, 300] {
            record_on(&c, "system", ts, "\"x\"").unwrap();
        }
        let removed = sweep_on(&c, "system", 250).unwrap();
        assert_eq!(removed, 2);
        let left: Vec<i64> = query_on(&c, "system", 0, i64::MAX, 10)
            .unwrap()
            .samples
            .iter()
            .map(|s| s.ts_ms)
            .collect();
        assert_eq!(left, vec![300]);
    }

    #[test]
    fn retention_override_roundtrip() {
        let c = mem();
        assert_eq!(retention_on(&c, "system").unwrap(), None);
        set_retention_on(&c, "system", Some(3_600_000)).unwrap();
        assert_eq!(retention_on(&c, "system").unwrap(), Some(3_600_000));
        set_retention_on(&c, "system", None).unwrap();
        assert_eq!(retention_on(&c, "system").unwrap(), None);
    }
}
