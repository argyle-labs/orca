//! Periodic housekeeping for unbounded on-disk state.
//!
//! Two loops share this module:
//!
//!   * **sweep** (hourly) — TTL/retention on slow-growing accretions:
//!     `update::prune_check_cache` (14-day `--check` blob TTL),
//!     `sweep_session_events` (audit log), and `sweep_expired_pod_offers`
//!     (dead pairing offers). These bound *live row* growth.
//!
//!   * **db-size** (every 10 min) — keeps the database FILE small and loud
//!     about it. SQLite frees pages on delete but does NOT return them to the
//!     OS unless vacuumed; a long-lived daemon that never vacuums can hold a
//!     multi-GB file over a few MB of live data (observed: 6.3 GB file, 4 MB
//!     data). Each pass reclaims freed pages (`incremental_vacuum`), flushes
//!     the WAL (`wal_checkpoint(TRUNCATE)`), runs a full `VACUUM` when the file
//!     crosses a size/bloat threshold, and emits a **loud warning** when the
//!     file is too big or the WAL won't flush.
//!
//! Both use the shared [`db::pool`] connection (opened once at startup) rather
//! than `open_default()` per tick, so maintenance stays reliable even if
//! fresh connection opens degrade on a busy host.
//!
//! Failures are best-effort: they log and the loop keeps running.

use std::time::Duration;

use notifications::{Event, EventClass, Severity};

use crate::periodic::{self, PeriodicSpec};

/// Cadence for the retention sweep. Hourly — these accretions grow over days.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
/// Cadence for the db-size guard. Every 10 min: the cheap reclaim/checkpoint
/// work is light, and we want the file kept small (and warnings surfaced)
/// promptly, not once an hour.
const DB_SIZE_INTERVAL: Duration = Duration::from_secs(600);

/// Pages to reclaim per incremental-vacuum pass. 4096 pages ≈ 16 MB at the
/// 4 KiB page size — plenty to keep pace with normal churn without a long lock.
const INCREMENTAL_VACUUM_PAGES: u32 = 4096;

// ── Size thresholds (bytes). Defaults tuned for "keep it VERY small"; each
// is overridable at runtime via the `settings` table (see `threshold`). ──

/// Emit a loud warning above this size (default 100 MiB). A healthy orca.db is
/// a few MB — 100 MB means something is retaining or not flushing.
const DEFAULT_WARN_BYTES: i64 = 100 * 1024 * 1024;
/// Force a full VACUUM above this size (default 250 MiB) regardless of ratio.
const DEFAULT_MAX_BYTES: i64 = 250 * 1024 * 1024;
/// Below this size, never bother with a full VACUUM even if the free ratio is
/// high — the reclaim isn't worth the write lock on a small file (default 20 MiB).
const DEFAULT_MIN_VACUUM_BYTES: i64 = 20 * 1024 * 1024;
/// Free-page ratio that triggers a full VACUUM (once above MIN_VACUUM_BYTES).
/// 0.25 = a quarter of the file is reclaimable dead space.
const FREE_RATIO_TRIGGER: f64 = 0.25;

/// Register both maintenance loops on the periodic scheduler. Idempotent.
pub fn spawn_periodic() {
    use std::sync::OnceLock;
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.sweep",
            // Stagger off the startup burst; the first sweep can wait a minute.
            initial_delay: Duration::from_secs(60),
            interval: SWEEP_INTERVAL,
        },
        periodic::boxed(sweep_tick),
    ));
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.db_size",
            initial_delay: Duration::from_secs(90),
            interval: DB_SIZE_INTERVAL,
        },
        periodic::boxed(db_size_tick),
    ));
}

async fn sweep_tick() -> anyhow::Result<()> {
    // Filesystem + DB work — run off the async reactor.
    tokio::task::spawn_blocking(|| {
        crate::update::prune_check_cache();
        let r = db::pool::with_pooled_or_open(|conn| {
            let days = db::maintenance::session_events_retention_days(conn);
            if let Err(e) = db::maintenance::sweep_session_events(conn, days) {
                tracing::debug!("[maintenance] sweep_session_events: {e:#}");
            }
            match db::maintenance::sweep_expired_pod_offers(conn) {
                Ok(n) if n > 0 => {
                    tracing::info!("[maintenance] swept {n} expired pairing offer(s)")
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("[maintenance] sweep_expired_pod_offers: {e:#}"),
            }
            Ok(())
        });
        if let Err(e) = r {
            tracing::debug!("[maintenance] db open for sweep failed: {e:#}");
        }
    })
    .await?;
    Ok(())
}

/// Outcome of one db-size pass, carried out of the blocking closure so the
/// async layer can emit notifications (which `emit` requires an await for).
#[derive(Default)]
struct DbSizePass {
    warnings: Vec<Event>,
}

async fn db_size_tick() -> anyhow::Result<()> {
    let pass = tokio::task::spawn_blocking(db_size_pass).await?;
    for ev in &pass.warnings {
        // Fan out to whatever notification backends are configured; a host
        // with none still gets the tracing::warn emitted in `db_size_pass`.
        let _ = notifications::emit(ev).await;
    }
    Ok(())
}

/// One synchronous db-size maintenance pass over the pooled connection.
/// Reclaims freed pages, flushes the WAL, full-VACUUMs on threshold, and
/// builds any loud warnings to emit. Never returns an error — a maintenance
/// hiccup must not kill the loop; problems are logged.
fn db_size_pass() -> DbSizePass {
    let mut pass = DbSizePass::default();
    let r = db::pool::with_pooled_or_open(|conn| {
        let warn_bytes = threshold(conn, "db.maintenance.warn_bytes", DEFAULT_WARN_BYTES);
        let max_bytes = threshold(conn, "db.maintenance.max_bytes", DEFAULT_MAX_BYTES);

        // 1) Reclaim already-freed pages (cheap; no-op until a full VACUUM has
        //    activated incremental auto-vacuum on this file).
        if let Err(e) = db::maintenance::incremental_vacuum(conn, INCREMENTAL_VACUUM_PAGES) {
            tracing::debug!("[maintenance] incremental_vacuum: {e:#}");
        }

        // 2) Flush the WAL back into the main db and truncate the -wal file.
        //    busy != 0 means a reader/writer blocked the checkpoint — the WAL
        //    is not fully flushing, which is worth a loud warning.
        let wal_busy = match db::maintenance::wal_checkpoint_truncate(conn) {
            Ok((busy, wal_pages, ckpt)) => {
                tracing::debug!(
                    "[maintenance] wal_checkpoint: busy={busy} wal_pages={wal_pages} checkpointed={ckpt}"
                );
                busy != 0
            }
            Err(e) => {
                tracing::debug!("[maintenance] wal_checkpoint: {e:#}");
                false
            }
        };

        // 3) Measure, and full-VACUUM if the file is over the hard cap or is
        //    bloated with reclaimable free space above the floor.
        let size = db::maintenance::db_size(conn)?;
        let bloated =
            size.total_bytes >= DEFAULT_MIN_VACUUM_BYTES && size.free_ratio() >= FREE_RATIO_TRIGGER;
        if size.total_bytes >= max_bytes || bloated {
            tracing::info!(
                "[maintenance] full VACUUM: total={} MiB free_ratio={:.2} (over_cap={} bloated={})",
                size.total_bytes / 1_048_576,
                size.free_ratio(),
                size.total_bytes >= max_bytes,
                bloated
            );
            if let Err(e) = db::maintenance::vacuum(conn) {
                tracing::warn!("[maintenance] full VACUUM failed: {e:#}");
            }
        }

        // 4) Re-measure and raise loud warnings on the post-maintenance state.
        let after = db::maintenance::db_size(conn)?;
        // Low-noise heartbeat (every DB_SIZE_INTERVAL) so db size is visible in
        // the log without hunting; the loud warnings below escalate on trouble.
        tracing::info!(
            "[maintenance] db_size: {} MiB used, free {:.0}% (warn≥{} MiB, cap≥{} MiB)",
            after.total_bytes / 1_048_576,
            after.free_ratio() * 100.0,
            warn_bytes / 1_048_576,
            max_bytes / 1_048_576,
        );
        if after.total_bytes >= warn_bytes {
            let mb = after.total_bytes / 1_048_576;
            tracing::warn!(
                "[maintenance] orca.db is {mb} MiB (warn threshold {} MiB) — free_ratio {:.2}",
                warn_bytes / 1_048_576,
                after.free_ratio()
            );
            pass.warnings.push(
                Event::new(
                    EventClass::Alert,
                    Severity::Warn,
                    format!("orca.db is {mb} MiB"),
                    "system.maintenance.db_size",
                )
                .with_body(format!(
                    "Database file is {mb} MiB (warn threshold {} MiB). Reclaimable free space {:.0}%. Check retention on high-churn tables (`db.stats`).",
                    warn_bytes / 1_048_576,
                    after.free_ratio() * 100.0
                )),
            );
        }
        if wal_busy {
            tracing::warn!(
                "[maintenance] WAL checkpoint blocked — write-ahead log is not flushing"
            );
            pass.warnings.push(
                Event::new(
                    EventClass::Alert,
                    Severity::Warn,
                    "orca.db WAL not flushing".to_string(),
                    "system.maintenance.db_size",
                )
                .with_body(
                    "A WAL checkpoint was blocked by an active reader/writer; the -wal file may grow unbounded. Investigate long-lived transactions.".to_string(),
                ),
            );
        }
        Ok(())
    });
    if let Err(e) = r {
        tracing::warn!("[maintenance] db_size pass failed: {e:#}");
    }
    pass
}

/// Resolve an integer size threshold: `settings` override if present and
/// parseable, else the compiled default.
fn threshold(conn: &rusqlite::Connection, key: &str, default: i64) -> i64 {
    match db::settings::get(conn, key) {
        Ok(Some(v)) => v.trim().parse::<i64>().unwrap_or(default),
        _ => default,
    }
}
