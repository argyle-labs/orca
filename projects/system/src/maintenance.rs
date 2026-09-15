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

use std::path::PathBuf;
use std::time::Duration;

use notifications::{Event, EventClass, Severity};

use crate::periodic::{self, PeriodicSpec};
use crate::remediation;

/// Cadence for the retention sweep. Hourly — these accretions grow over days.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
/// Cadence for the db-size guard. Every 10 min: the cheap reclaim/checkpoint
/// work is light, and we want the file kept small (and warnings surfaced)
/// promptly, not once an hour.
const DB_SIZE_INTERVAL: Duration = Duration::from_secs(600);
/// Cadence for the local disk-usage alert check. Every 10 min: a filling
/// filesystem (the gitea/actcache silent-fill class) should be surfaced well
/// before it wedges, and the sysinfo probe is cheap.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(600);
/// Cadence for the baseline-plugin reconcile. Every 15 min: the check is a
/// cheap in-memory/on-disk lookup when the plugin is present (the steady state),
/// and a still-absent host naturally retries on the next tick.
const BASELINE_PLUGIN_INTERVAL: Duration = Duration::from_secs(900);

/// The one plugin that must be present on every daemon: the orca web UI. If it
/// is absent, the baseline reconcile installs it from the catalog.
const BASELINE_PLUGIN_NAME: &str = "peacock";

/// Percent-full at which a filesystem earns a `Warn` alert (default 85).
/// Overridable via the `settings` key `disk.alert.warn_pct`.
const DEFAULT_DISK_WARN_PCT: i64 = 85;
/// Percent-full at which a filesystem earns a `Critical` alert (default 95).
/// Overridable via the `settings` key `disk.alert.crit_pct`.
const DEFAULT_DISK_CRIT_PCT: i64 = 95;
/// Persisted-state key prefix: the last-alerted level for a given mount point,
/// e.g. `disk_alert_level:/`. Absent = `Ok`. Used to rate-limit so a steady
/// near-full mount alerts once on the way up, not every tick.
const DISK_ALERT_LEVEL_PREFIX: &str = "disk_alert_level:";

/// Compiled default cache cap (GiB) for the runner-cache tick when
/// `ci.runner.cache.cap_gb` is unset. Overridable via that settings key.
const DEFAULT_RUNNER_CACHE_CAP_GB: i64 = 10;

// ── Runner-wedge detection (opt-in, policy-gated) ────────────────────────────
//
// Detect a gitea-actions job container that has been Up past a wall-clock
// ceiling while making ~0 CPU progress (the frozen-rustup-holding-a-runner-slot
// signature) and remediate under the host RemediationPolicy: notify by default,
// `docker kill` only when the policy `acts()`. Kill is confined to containers
// whose NAME starts with the configured prefix — the safety boundary.

/// Default container-name prefix for gitea-actions job containers. A candidate
/// is considered ONLY if its name starts with this (or the override). An
/// empty/whitespace prefix matches NOTHING, never everything.
const DEFAULT_WEDGE_PREFIX: &str = "GITEA-ACTIONS-TASK-";
/// Default wall-clock age (minutes) a candidate must exceed to be wedge-eligible.
const DEFAULT_WEDGE_MAX_AGE_MIN: i64 = 30;
/// Default consecutive low-CPU ticks required before a candidate is wedged.
/// 2 ticks at the 600s interval ≈ ~20 min of no progress — past a normal step gap.
const DEFAULT_WEDGE_STALL_TICKS: i64 = 2;
/// Default CPU-percent floor: a reading strictly below this counts as "no progress".
const DEFAULT_WEDGE_MIN_CPU_PCT: i64 = 1;
/// State-key prefix: per-container low-CPU streak, e.g. `wedge:<id>` → `u32` TEXT.
const WEDGE_STREAK_PREFIX: &str = "wedge:";
/// State-key prefix: per-container once-per-wedge dedup marker, e.g.
/// `wedge_alerted:<id>`. Set on act/notify, cleared when the container is gone.
const WEDGE_ALERTED_PREFIX: &str = "wedge_alerted:";

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
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.disk",
            initial_delay: Duration::from_secs(120),
            interval: DISK_CHECK_INTERVAL,
        },
        periodic::boxed(disk_tick),
    ));
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.guest_disk",
            // Stagger 180s after the 120s host disk tick so the two checks don't
            // collide on startup; a silent no-op on non-proxmox hosts anyway.
            initial_delay: Duration::from_secs(180),
            interval: DISK_CHECK_INTERVAL,
        },
        periodic::boxed(guest_disk_tick),
    ));
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.runner_cache",
            // Stagger 240s after the other disk ticks so the walk doesn't collide
            // on startup; a silent no-op unless `ci.runner.cache.dir` is set.
            initial_delay: Duration::from_secs(240),
            interval: DISK_CHECK_INTERVAL,
        },
        periodic::boxed(runner_cache_tick),
    ));
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.runner_wedge",
            // Stagger 300s after the runner-cache tick so the docker probes don't
            // collide on startup; a silent no-op unless `ci.runner.wedge.enabled`.
            initial_delay: Duration::from_secs(300),
            interval: DISK_CHECK_INTERVAL,
        },
        periodic::boxed(runner_wedge_tick),
    ));
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.baseline_plugin",
            // Run shortly after startup so a fresh host converges quickly, but
            // after the startup burst and the plugin startup-scan have settled.
            initial_delay: Duration::from_secs(150),
            interval: BASELINE_PLUGIN_INTERVAL,
        },
        periodic::boxed(baseline_plugin_tick),
    ));
}

/// Decide whether the baseline reconcile should install the plugin this tick.
/// Pure seam over the "is it already present?" probe: present → no-op, absent →
/// install. Kept separate so the decision is unit-testable without touching the
/// real catalog/network install path.
fn should_install_baseline(present: bool) -> bool {
    !present
}

/// Is the baseline plugin already present — either live-loaded or on disk (so a
/// host that has it installed-but-not-yet-loaded still counts)? Cheap: an
/// in-memory registry scan plus a directory read, no network.
fn baseline_plugin_present(name: &str) -> bool {
    plugin_loader::loaded_plugins()
        .iter()
        .any(|p| p.software == name)
        || crate::plugin_manager::installed_software_on_disk()
            .iter()
            .any(|s| s == name)
}

/// Ensure the baseline web-UI plugin is installed. Install-if-absent only:
/// keeping it at channel-latest is handled by the update paths, not here.
/// Best-effort — any failure (config load, unreachable catalog, fetch error) is
/// logged and swallowed so the maintenance loop never wedges; the next tick
/// retries while the plugin is still absent.
async fn baseline_plugin_tick() -> anyhow::Result<()> {
    if !should_install_baseline(baseline_plugin_present(BASELINE_PLUGIN_NAME)) {
        return Ok(());
    }
    // Match update resolution: a beta-channel host installs the prerelease line.
    let prerelease = matches!(
        crate::update_state::read_channel_marker(),
        Some(crate::update_state::Channel::Beta)
    );
    let ctx = match contract::config::Config::load() {
        Ok(cfg) => contract::ToolCtx::new(std::sync::Arc::new(cfg)),
        Err(e) => {
            tracing::warn!("[maintenance] baseline plugin: config load failed: {e:#}");
            return Ok(());
        }
    };
    match crate::plugin_manager::install_from_catalog(BASELINE_PLUGIN_NAME, None, prerelease, &ctx)
        .await
    {
        Ok(out) => tracing::info!(
            "[maintenance] installed baseline plugin {} v{}",
            out.software,
            out.version
        ),
        Err(e) => {
            tracing::warn!("[maintenance] baseline plugin install (best-effort): {e:#}")
        }
    }
    Ok(())
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
            // Compact the delete command-log: ops past the anti-entropy horizon
            // have propagated to every online peer (a longer-offline host
            // re-bootstraps from a snapshot), so the death record is no longer
            // load-bearing. Keeps "delete means delete" true for the log itself.
            let now_ms = utils::time::now_millis_since_epoch();
            match db::replication_ops::reap(conn, now_ms, db::replication_ops::DEFAULT_TTL_MS) {
                Ok(n) if n > 0 => tracing::info!("[maintenance] reaped {n} replication op(s)"),
                Ok(_) => {}
                Err(e) => tracing::debug!("[maintenance] replication_ops reap: {e:#}"),
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
                    "Database file is {mb} MiB (warn threshold {} MiB). Reclaimable free space {:.0}%. Check retention on high-churn tables (`db.detail --view stats`).",
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

// ── Local disk-usage alerting ────────────────────────────────────────────────
//
// Each daemon watches its OWN local filesystems and emits a threshold alert
// through the notification dispatcher when a mount crosses `warn`/`crit`. This
// covers orca *host* self-monitoring only; managed guests that don't run orca
// (LXC/VM) are a documented follow-up (probed via the managing host).

/// Threshold band a filesystem's used-percent falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskLevel {
    Ok,
    Warn,
    Critical,
}

impl DiskLevel {
    /// Persisted-state token (mirror of `parse`).
    fn as_token(self) -> &'static str {
        match self {
            DiskLevel::Ok => "ok",
            DiskLevel::Warn => "warn",
            DiskLevel::Critical => "critical",
        }
    }

    /// Parse a stored token; unknown/absent values read as `Ok`.
    fn parse(s: &str) -> DiskLevel {
        match s.trim() {
            "warn" => DiskLevel::Warn,
            "critical" => DiskLevel::Critical,
            _ => DiskLevel::Ok,
        }
    }

    /// Ordering rank so increases/decreases are comparable.
    fn rank(self) -> u8 {
        match self {
            DiskLevel::Ok => 0,
            DiskLevel::Warn => 1,
            DiskLevel::Critical => 2,
        }
    }
}

/// Map a used-percent to a level given the `warn`/`crit` thresholds.
fn level_for(used_pct: i64, warn: i64, crit: i64) -> DiskLevel {
    if used_pct >= crit {
        DiskLevel::Critical
    } else if used_pct >= warn {
        DiskLevel::Warn
    } else {
        DiskLevel::Ok
    }
}

/// Rate-limit rule: emit only when the level increases (Ok→Warn, Warn→Crit,
/// Ok→Crit) or when it fully recovers to Ok (a one-shot recovery notice).
/// Never emit while unchanged, and never on a partial drop (Crit→Warn).
fn should_emit(prev: DiskLevel, new: DiskLevel) -> bool {
    if new == prev {
        return false;
    }
    new.rank() > prev.rank() || new == DiskLevel::Ok
}

/// One filesystem's measured state, carried out of the blocking probe.
struct DiskUsage {
    mount: String,
    used_pct: i64,
    avail_gb: u64,
    total_gb: u64,
}

/// A notification to emit plus the mount + level to persist once emitted.
struct DiskAlert {
    mount: String,
    level: DiskLevel,
    event: Event,
}

async fn disk_tick() -> anyhow::Result<()> {
    let alerts = tokio::task::spawn_blocking(disk_pass).await?;
    for a in &alerts {
        // Best-effort fan-out: a host with no backends configured gets an empty
        // vec (harmless no-op); the persist below still records the new level.
        let _ = notifications::emit(&a.event).await;
        let key = format!("{DISK_ALERT_LEVEL_PREFIX}{}", a.mount);
        let r = db::pool::with_pooled_or_open(|conn| {
            if a.level == DiskLevel::Ok {
                db::settings::delete(conn, &key)?;
            } else {
                db::settings::set(conn, &key, a.level.as_token())?;
            }
            Ok(())
        });
        if let Err(e) = r {
            tracing::debug!("[maintenance] disk alert persist for {}: {e:#}", a.mount);
        }
    }
    Ok(())
}

/// Enumerate local real filesystems, compute their level, and build the alerts
/// that must be emitted this pass (level increased, or recovered to Ok). Never
/// errors — a probe hiccup must not kill the loop; problems are logged.
fn disk_pass() -> Vec<DiskAlert> {
    let host = sysinfo::System::host_name().unwrap_or_else(|| "this host".to_string());
    let (warn, crit) = db::pool::with_pooled_or_open(|conn| {
        Ok((
            threshold(conn, "disk.alert.warn_pct", DEFAULT_DISK_WARN_PCT),
            threshold(conn, "disk.alert.crit_pct", DEFAULT_DISK_CRIT_PCT),
        ))
    })
    .unwrap_or((DEFAULT_DISK_WARN_PCT, DEFAULT_DISK_CRIT_PCT));

    let mut alerts = Vec::new();
    for u in probe_local_disks() {
        let new = level_for(u.used_pct, warn, crit);
        let key = format!("{DISK_ALERT_LEVEL_PREFIX}{}", u.mount);
        let prev = db::pool::with_pooled_or_open(|conn| {
            Ok(DiskLevel::parse(
                db::settings::get(conn, &key)?.as_deref().unwrap_or(""),
            ))
        })
        .unwrap_or(DiskLevel::Ok);

        if !should_emit(prev, new) {
            continue;
        }
        let (class, severity) = match new {
            DiskLevel::Ok => (EventClass::Alert, Severity::Info),
            DiskLevel::Warn => (EventClass::Alert, Severity::Warn),
            DiskLevel::Critical => (EventClass::Alert, Severity::Critical),
        };
        let title = if new == DiskLevel::Ok {
            format!("disk recovered on {host}:{}", u.mount)
        } else {
            format!("disk {}% on {host}:{}", u.used_pct, u.mount)
        };
        tracing::warn!(
            "[maintenance] disk {}% on {host}:{} ({} GB free / {} GB) — {prev:?}→{new:?}",
            u.used_pct,
            u.mount,
            u.avail_gb,
            u.total_gb
        );
        let event = Event::new(class, severity, title, "system.maintenance.disk")
            .with_host(host.clone())
            .with_body(format!(
                "Filesystem {} on {host} is {}% full ({} GB free of {} GB).",
                u.mount, u.used_pct, u.avail_gb, u.total_gb
            ));
        alerts.push(DiskAlert {
            mount: u.mount,
            level: new,
            event,
        });
    }
    alerts
}

/// Probe local, non-removable filesystems via sysinfo. Skips removable media
/// and zero-total pseudo filesystems; deduplicates by mount point.
fn probe_local_disks() -> Vec<DiskUsage> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in disks.list() {
        if d.is_removable() {
            continue;
        }
        let total = d.total_space();
        if total == 0 {
            continue;
        }
        let mount = d.mount_point().display().to_string();
        if !seen.insert(mount.clone()) {
            continue;
        }
        let avail = d.available_space();
        let used_pct = (((total - avail) as f64 / total as f64) * 100.0).round() as i64;
        out.push(DiskUsage {
            mount,
            used_pct,
            avail_gb: avail / 1024 / 1024 / 1024,
            total_gb: total / 1024 / 1024 / 1024,
        });
    }
    out
}

// ── Guest disk-usage alerting (proxmox-managed LXC) ──────────────────────────
//
// A proxmox host also watches the rootfs of each LXC guest it hosts — the guests
// that don't run orca and so can't self-monitor (the silent-fill class that
// wedged a non-orca guest). It polls `df` inside each guest over the allowlisted
// `guest.exec` seam and alerts on the SAME warn/crit thresholds as the host path.
// Strictly additive/observational: any guest probe failure is logged and skipped,
// never propagated — orca must never be able to wedge a host it manages.

/// Parse `df -P /` output into the rootfs usage. `df -P` guarantees one data row
/// with columns `Filesystem 1024-blocks Used Available Capacity Mounted-on`; we
/// take used-percent from `Capacity` (trailing `%` stripped) and total/avail from
/// the 1024-block counts (KiB → GiB, saturating). Defensive: any shape mismatch
/// (no data line, too few columns, unparseable numbers) yields `None`.
fn parse_df_root(stdout: &str) -> Option<DiskUsage> {
    // Skip the header line; take the first non-empty data row.
    let row = stdout.lines().skip(1).find(|l| !l.trim().is_empty())?;
    let cols: Vec<&str> = row.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }
    let total_kib: u64 = cols[1].parse().ok()?;
    let avail_kib: u64 = cols[3].parse().ok()?;
    let used_pct: i64 = cols[4].trim_end_matches('%').parse().ok()?;
    Some(DiskUsage {
        mount: cols[5].to_string(),
        used_pct,
        avail_gb: avail_kib / 1024 / 1024,
        total_gb: total_kib / 1024 / 1024,
    })
}

/// Per-guest state-key for the last-alerted level. A distinct `guest:<vmid>:`
/// namespace under the shared prefix so it never collides with a host mount key.
fn guest_disk_key(vmid: &str, mount: &str) -> String {
    format!("{DISK_ALERT_LEVEL_PREFIX}guest:{vmid}:{mount}")
}

/// Poll `df` inside each LXC guest this proxmox host manages and alert on high
/// rootfs usage. Best-effort per guest: any error/non-zero exit/timeout/malformed
/// output is logged and skipped — a single guest's failure never aborts the tick,
/// and this never panics. Silent no-op on non-proxmox hosts (the common case).
async fn guest_disk_tick() -> anyhow::Result<()> {
    // Most hosts don't manage LXC guests — bail cheaply and quietly.
    if !crate::capability::is_available("proxmox") {
        return Ok(());
    }

    let guests: Vec<(String, String)> = crate::topology::collect_claims()
        .await
        .into_iter()
        .filter(|c| c.kind == "lxc")
        .map(|c| (c.id, c.name))
        .collect();
    if guests.is_empty() {
        return Ok(());
    }

    let (warn, crit) = db::pool::with_pooled_or_open(|conn| {
        Ok((
            threshold(conn, "disk.alert.warn_pct", DEFAULT_DISK_WARN_PCT),
            threshold(conn, "disk.alert.crit_pct", DEFAULT_DISK_CRIT_PCT),
        ))
    })
    .unwrap_or((DEFAULT_DISK_WARN_PCT, DEFAULT_DISK_CRIT_PCT));

    // Sequential fan-out: the guest count per host is small and simplicity beats
    // concurrency here (no extra `futures` dependency).
    for (vmid, name) in guests {
        let req = contract::guest_exec::ExecRequest {
            command: vec!["df".into(), "-P".into(), "/".into()],
            timeout_ms: Some(10_000),
            ..Default::default()
        };
        let guest = contract::guest_exec::GuestRef {
            id: vmid.clone(),
            ..Default::default()
        };
        let out =
            match contract::guest_exec::exec(crate::guest_exec_provider::PROVIDER_NAME, guest, req)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::debug!("[maintenance] guest_disk exec on {vmid} ({name}): {e:#}");
                    continue;
                }
            };
        if out.timed_out || out.exit_code != Some(0) {
            tracing::debug!(
                "[maintenance] guest_disk df on {vmid} ({name}): exit={:?} timed_out={}",
                out.exit_code,
                out.timed_out
            );
            continue;
        }
        let usage = match parse_df_root(&out.stdout) {
            Some(u) => u,
            None => {
                tracing::warn!(
                    "[maintenance] guest_disk df on {vmid} ({name}): unparseable output"
                );
                continue;
            }
        };

        let new = level_for(usage.used_pct, warn, crit);
        let key = guest_disk_key(&vmid, &usage.mount);
        let prev = db::pool::with_pooled_or_open(|conn| {
            Ok(DiskLevel::parse(
                db::settings::get(conn, &key)?.as_deref().unwrap_or(""),
            ))
        })
        .unwrap_or(DiskLevel::Ok);
        if !should_emit(prev, new) {
            continue;
        }

        let (class, severity) = match new {
            DiskLevel::Ok => (EventClass::Alert, Severity::Info),
            DiskLevel::Warn => (EventClass::Alert, Severity::Warn),
            DiskLevel::Critical => (EventClass::Alert, Severity::Critical),
        };
        let host = format!("guest:{vmid} ({name})");
        let title = if new == DiskLevel::Ok {
            format!("disk recovered on guest {vmid} ({name}):{}", usage.mount)
        } else {
            format!(
                "disk {}% on guest {vmid} ({name}):{}",
                usage.used_pct, usage.mount
            )
        };
        tracing::warn!(
            "[maintenance] guest disk {}% on {vmid} ({name}):{} ({} GB free / {} GB) — {prev:?}→{new:?}",
            usage.used_pct,
            usage.mount,
            usage.avail_gb,
            usage.total_gb
        );
        let event = Event::new(class, severity, title, "system.maintenance.guest_disk")
            .with_host(host)
            .with_body(format!(
                "Guest {vmid} ({name}) filesystem {} is {}% full ({} GB free of {} GB).",
                usage.mount, usage.used_pct, usage.avail_gb, usage.total_gb
            ));
        // Best-effort fan-out, then persist the new level (mirrors the host path).
        let _ = notifications::emit(&event).await;
        let r = db::pool::with_pooled_or_open(|conn| {
            if new == DiskLevel::Ok {
                db::settings::delete(conn, &key)?;
            } else {
                db::settings::set(conn, &key, new.as_token())?;
            }
            Ok(())
        });
        if let Err(e) = r {
            tracing::debug!("[maintenance] guest disk alert persist for {vmid}: {e:#}");
        }
    }
    Ok(())
}

// ── Gitea runner cache bounding (opt-in, unprivileged) ───────────────────────
//
// Replaces a host-local cron that capped a gitea act_runner cache dir and pruned
// dangling docker images. Opt-in per host via the `ci.runner.cache.dir` setting;
// unset ⇒ silent no-op (the common case). UNPRIVILEGED first cut: the daemon runs
// as a non-root user while the actcache is often root-owned, so trimming is
// best-effort — a permission-denied delete is logged and skipped, but the tick
// STILL emits an over-budget alert so an un-trimmable cache stays visible. A
// privileged delete seam (e.g. via a root helper) is a deferred follow-up.

/// One cache-file candidate for the trim planner. Pure data — no fs/time I/O so
/// the planner is unit-testable over synthetic entries.
struct TrimEntry {
    path: PathBuf,
    size: u64,
    mtime_unix: u64,
}

/// Sum the sizes of a set of cache entries.
fn total_size(entries: &[TrimEntry]) -> u64 {
    entries.iter().map(|e| e.size).sum()
}

/// Pure trim planner: given the walked cache entries and a byte cap, pick the
/// OLDEST-first (ascending mtime) files to delete until the remaining total is
/// at or under `cap_bytes`. Returns their paths in deletion order. Under-budget
/// (including equal-to-cap) ⇒ empty. Never returns a path outside `entries`, so
/// deletion is confined to the operator-configured cache dir.
fn plan_trim(entries: &[TrimEntry], cap_bytes: u64) -> Vec<PathBuf> {
    let mut total = total_size(entries);
    if total <= cap_bytes {
        return Vec::new();
    }
    // Stable sort by mtime keeps a deterministic tie-break (input order).
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by_key(|&i| entries[i].mtime_unix);
    let mut out = Vec::new();
    for i in idx {
        if total <= cap_bytes {
            break;
        }
        out.push(entries[i].path.clone());
        total = total.saturating_sub(entries[i].size);
    }
    out
}

/// Post-trim size band for the cache dir. Direct byte comparison (no percent
/// semantics): `Ok` at/under cap, `Warn` up to 2×cap, `Critical` beyond. A
/// `cap_bytes` of 0 is guarded — anything non-empty is `Critical`, empty is `Ok`.
fn cache_level(post_bytes: u64, cap_bytes: u64) -> DiskLevel {
    if cap_bytes == 0 {
        return if post_bytes == 0 {
            DiskLevel::Ok
        } else {
            DiskLevel::Critical
        };
    }
    if post_bytes <= cap_bytes {
        DiskLevel::Ok
    } else if post_bytes <= cap_bytes.saturating_mul(2) {
        DiskLevel::Warn
    } else {
        DiskLevel::Critical
    }
}

/// Recursively collect regular files under `dir` into `TrimEntry`s. Does NOT
/// follow directory symlinks (guards against symlink loops); per-entry metadata
/// errors skip that entry. Blocking `std::fs` — run under `spawn_blocking`.
fn collect_cache_entries(dir: &std::path::Path) -> Vec<TrimEntry> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let path = ent.path();
            // symlink_metadata: never traverses a symlink, so a directory
            // symlink is treated as a leaf and not descended into.
            let md = match std::fs::symlink_metadata(&path) {
                Ok(md) => md,
                Err(_) => continue,
            };
            let ft = md.file_type();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let mtime_unix = md
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                out.push(TrimEntry {
                    path,
                    size: md.len(),
                    mtime_unix,
                });
            }
        }
    }
    out
}

/// Bound the opt-in gitea runner cache dir and prune dangling docker images.
/// Best-effort throughout: never `?`-propagates a work error, never panics, and
/// silently no-ops on the majority of hosts (no `ci.runner.cache.dir` set).
async fn runner_cache_tick() -> anyhow::Result<()> {
    let (dir, cap_gb) = db::pool::with_pooled_or_open(|conn| {
        let dir = db::settings::get(conn, "ci.runner.cache.dir")?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let cap_gb = threshold(conn, "ci.runner.cache.cap_gb", DEFAULT_RUNNER_CACHE_CAP_GB);
        Ok((dir, cap_gb))
    })
    .unwrap_or((None, DEFAULT_RUNNER_CACHE_CAP_GB));

    // Unset/empty enable key ⇒ this host doesn't run a gitea runner: no-op.
    let dir = match dir {
        Some(d) => d,
        None => return Ok(()),
    };
    let cap_bytes = (cap_gb.max(0) as u64).saturating_mul(1024 * 1024 * 1024);

    // Walk + trim off the async reactor: enumerating a large cache dir blocks.
    let dir_for_walk = dir.clone();
    let (post_bytes, freed_bytes, removed, denied) = tokio::task::spawn_blocking(move || {
        let entries = collect_cache_entries(std::path::Path::new(&dir_for_walk));
        let doomed = plan_trim(&entries, cap_bytes);
        // Track which paths were actually removed so the post-trim size is exact.
        let mut removed_set = std::collections::HashSet::new();
        let mut freed_bytes: u64 = 0;
        let mut removed: u64 = 0;
        let mut denied: u64 = 0;
        // Index sizes by path for the freed-bytes accounting.
        let size_of: std::collections::HashMap<&std::path::Path, u64> =
            entries.iter().map(|e| (e.path.as_path(), e.size)).collect();
        for path in &doomed {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    removed += 1;
                    freed_bytes += size_of.get(path.as_path()).copied().unwrap_or(0);
                    removed_set.insert(path.clone());
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied += 1,
                Err(e) => tracing::debug!("[maintenance] runner_cache remove {path:?}: {e:#}"),
            }
        }
        if denied > 0 {
            tracing::warn!(
                "[maintenance] runner_cache: {denied} file(s) could not be removed (permission denied) — orca runs unprivileged; a root trim seam is a follow-up"
            );
        }
        let post_bytes: u64 = entries
            .iter()
            .filter(|e| !removed_set.contains(&e.path))
            .map(|e| e.size)
            .sum();
        (post_bytes, freed_bytes, removed, denied)
    })
    .await
    .unwrap_or((0, 0, 0, 0));

    // Prune dangling docker images (best-effort) only where docker is present.
    // NEVER `-a` — only untagged/dangling layers, bounded by a timeout.
    if crate::capability::is_available("docker") {
        let fut = tokio::process::Command::new("docker")
            .args(["image", "prune", "-f", "--filter", "dangling=true"])
            .output();
        match tokio::time::timeout(Duration::from_secs(30), fut).await {
            Ok(Ok(o)) if o.status.success() => {}
            Ok(Ok(o)) => tracing::debug!(
                "[maintenance] runner_cache docker prune exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Ok(Err(e)) => tracing::debug!("[maintenance] runner_cache docker prune spawn: {e:#}"),
            Err(_) => tracing::debug!("[maintenance] runner_cache docker prune timed out"),
        }
    }

    let new = cache_level(post_bytes, cap_bytes);
    let over_budget = new != DiskLevel::Ok;
    if over_budget || denied > 0 {
        tracing::warn!(
            "[maintenance] runner_cache {dir}: {} MiB after trim (cap {cap_gb} GiB), freed {} MiB / {removed} file(s), {denied} permission-denied — {new:?}",
            post_bytes / 1_048_576,
            freed_bytes / 1_048_576,
        );
    } else {
        tracing::info!(
            "[maintenance] runner_cache {dir}: {} MiB (cap {cap_gb} GiB), freed {} MiB / {removed} file(s)",
            post_bytes / 1_048_576,
            freed_bytes / 1_048_576,
        );
    }

    let key = format!("{DISK_ALERT_LEVEL_PREFIX}runner_cache:{dir}");
    let prev = db::pool::with_pooled_or_open(|conn| {
        Ok(DiskLevel::parse(
            db::settings::get(conn, &key)?.as_deref().unwrap_or(""),
        ))
    })
    .unwrap_or(DiskLevel::Ok);
    if should_emit(prev, new) {
        let (class, severity) = match new {
            DiskLevel::Ok => (EventClass::Alert, Severity::Info),
            DiskLevel::Warn => (EventClass::Alert, Severity::Warn),
            DiskLevel::Critical => (EventClass::Alert, Severity::Critical),
        };
        let host = sysinfo::System::host_name().unwrap_or_else(|| "this host".to_string());
        let title = if new == DiskLevel::Ok {
            format!("runner cache recovered on {host}:{dir}")
        } else {
            format!(
                "runner cache {} MiB on {host}:{dir}",
                post_bytes / 1_048_576
            )
        };
        let mut body = format!(
            "Gitea runner cache {dir} on {host} is {} MiB after trim (cap {cap_gb} GiB); freed {} MiB across {removed} file(s).",
            post_bytes / 1_048_576,
            freed_bytes / 1_048_576,
        );
        if denied > 0 {
            body.push_str(&format!(
                " {denied} file(s) could not be removed — orca lacks permission to trim (runs unprivileged; a root trim seam is a follow-up)."
            ));
        }
        let event = Event::new(class, severity, title, "system.maintenance.runner_cache")
            .with_host(host)
            .with_body(body);
        let _ = notifications::emit(&event).await;
    }
    let r = db::pool::with_pooled_or_open(|conn| {
        if new == DiskLevel::Ok {
            db::settings::delete(conn, &key)?;
        } else {
            db::settings::set(conn, &key, new.as_token())?;
        }
        Ok(())
    });
    if let Err(e) = r {
        tracing::debug!("[maintenance] runner_cache alert persist for {dir}: {e:#}");
    }
    Ok(())
}

// ── Runner-wedge pure detection helpers (unit-tested; no I/O) ────────────────

/// Whether `value` reads as truthy (opt-in gate). Anything else — including
/// None handled by the caller — is false.
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Does `name` identify a gitea-actions job container under `prefix`?
/// Startswith semantics, but an empty/whitespace prefix matches NOTHING — a
/// blank prefix (misconfig) must never select every container.
fn matches_runner_prefix(name: &str, prefix: &str) -> bool {
    if prefix.trim().is_empty() {
        return false;
    }
    name.starts_with(prefix)
}

/// Parse a `docker stats` CPU field like `"0.00%"` / `"12.50%"` into a percent.
/// Junk (missing `%`, non-numeric) → None.
fn parse_cpu_pct(field: &str) -> Option<f64> {
    field.trim().strip_suffix('%')?.trim().parse::<f64>().ok()
}

/// Next low-CPU streak: below the floor extends the streak, at/above resets it.
fn next_streak(prev: u32, cpu_pct: f64, min_cpu_pct: f64) -> u32 {
    if cpu_pct < min_cpu_pct {
        prev.saturating_add(1)
    } else {
        0
    }
}

/// Wedged verdict: old enough AND stalled long enough. `age > max` (equal is not
/// wedged) and `streak >= stall`.
fn is_wedged(age_secs: i64, max_age_secs: i64, streak: u32, stall_ticks: u32) -> bool {
    age_secs > max_age_secs && streak >= stall_ticks
}

/// Age in seconds from a container start time, saturating at 0 (never negative).
fn age_secs(started_unix: i64, now_unix: i64) -> i64 {
    (now_unix - started_unix).max(0)
}

/// Parse a container start time into unix seconds, trying the RFC3339
/// `docker inspect .State.StartedAt` form first, then the looser
/// `docker ps .CreatedAt` form (`YYYY-MM-DD HH:MM:SS ±HHMM TZ`). None on failure.
fn parse_container_start(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(ts) = utils::time::Timestamp::parse_rfc3339(raw) {
        return Some(ts.unix_seconds());
    }
    // docker ps CreatedAt: "2026-09-14 12:34:56 -0700 PDT" → RFC3339.
    let mut it = raw.split_whitespace();
    let date = it.next()?;
    let time = it.next()?;
    let off = it.next()?;
    let (sign, rest) = off.split_at(off.char_indices().nth(1)?.0);
    if (sign != "+" && sign != "-") || rest.len() != 4 {
        return None;
    }
    let (hh, mm) = rest.split_at(2);
    let rfc = format!("{date}T{time}{sign}{hh}:{mm}");
    utils::time::Timestamp::parse_rfc3339(&rfc)
        .ok()
        .map(|ts| ts.unix_seconds())
}

// ── Runner-wedge tick ────────────────────────────────────────────────────────

/// Config resolved once per tick.
struct WedgeConfig {
    enabled: bool,
    prefix: String,
    max_age_secs: i64,
    stall_ticks: u32,
    min_cpu_pct: f64,
}

/// A prefix-matched candidate container observed this tick.
struct WedgeObserved {
    id: String,
    name: String,
    started_unix: Option<i64>,
    cpu_pct: Option<f64>,
}

/// A wedged container that needs remediation this tick (not yet alerted).
struct WedgeAction {
    id: String,
    name: String,
    age_min: i64,
    cpu_pct: f64,
    streak: u32,
}

/// Detect and remediate wedged gitea-actions job containers. Layered kill guard:
/// (1) opt-in `ci.runner.wedge.enabled`, (2) container name matches the prefix,
/// (3) `remediation::policy().acts()`. Default policy is `notify` ⇒ detect +
/// notify, never kill. Best-effort throughout: never `?`-propagates a work
/// error, never panics, silent no-op on the majority of hosts.
async fn runner_wedge_tick() -> anyhow::Result<()> {
    let cfg = db::pool::with_pooled_or_open(|conn| {
        let enabled = db::settings::get(conn, "ci.runner.wedge.enabled")?
            .map(|v| is_truthy(&v))
            .unwrap_or(false);
        let prefix = db::settings::get(conn, "ci.runner.wedge.container_prefix")?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_WEDGE_PREFIX.to_string());
        let max_age_min = threshold(
            conn,
            "ci.runner.wedge.max_age_min",
            DEFAULT_WEDGE_MAX_AGE_MIN,
        );
        let stall_ticks = threshold(
            conn,
            "ci.runner.wedge.stall_ticks",
            DEFAULT_WEDGE_STALL_TICKS,
        );
        let min_cpu_pct = threshold(
            conn,
            "ci.runner.wedge.min_cpu_pct",
            DEFAULT_WEDGE_MIN_CPU_PCT,
        );
        Ok(WedgeConfig {
            enabled,
            prefix,
            max_age_secs: max_age_min.max(0) * 60,
            stall_ticks: stall_ticks.max(0) as u32,
            min_cpu_pct: min_cpu_pct as f64,
        })
    })
    .unwrap_or(WedgeConfig {
        enabled: false,
        prefix: DEFAULT_WEDGE_PREFIX.to_string(),
        max_age_secs: DEFAULT_WEDGE_MAX_AGE_MIN * 60,
        stall_ticks: DEFAULT_WEDGE_STALL_TICKS as u32,
        min_cpu_pct: DEFAULT_WEDGE_MIN_CPU_PCT as f64,
    });

    // Guard 1: opt-in. Most hosts aren't gitea runners.
    if !cfg.enabled {
        return Ok(());
    }
    // A blank prefix would match nothing anyway (matches_runner_prefix), but bail
    // early — no point probing docker when no container could ever be a candidate.
    if cfg.prefix.trim().is_empty() {
        return Ok(());
    }
    if !crate::capability::is_available("docker") {
        return Ok(());
    }

    // Enumerate candidate containers (name prefix-matched) and their CPU%.
    let observed = observe_wedge_candidates(&cfg).await;

    // Read policy + update per-container streaks/dedup in one sync pass; also
    // clean up state rows for containers that have since disappeared.
    let now_unix = utils::time::now_secs_since_epoch();
    let (policy, actions) = db::pool::with_pooled_or_open(|conn| {
        let policy = remediation::policy(conn).unwrap_or_else(|e| {
            tracing::warn!(
                "[maintenance] runner_wedge: policy read failed ({e}); defaulting to notify"
            );
            remediation::RemediationPolicy::default()
        });
        let mut actions: Vec<WedgeAction> = Vec::new();
        let present: std::collections::HashSet<&str> =
            observed.iter().map(|o| o.id.as_str()).collect();

        for o in &observed {
            let streak_key = format!("{WEDGE_STREAK_PREFIX}{}", o.id);
            let prev: u32 = db::settings::get(conn, &streak_key)?
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            // Unknown CPU (missing stats row) can't confirm "no progress" → reset,
            // so a container is never killed on a reading we couldn't take.
            let streak = match o.cpu_pct {
                Some(cpu) => next_streak(prev, cpu, cfg.min_cpu_pct),
                None => 0,
            };
            db::settings::set(conn, &streak_key, &streak.to_string())?;

            let age = o.started_unix.map(|s| age_secs(s, now_unix)).unwrap_or(0);
            let wedged = o.started_unix.is_some()
                && is_wedged(age, cfg.max_age_secs, streak, cfg.stall_ticks);

            let alerted_key = format!("{WEDGE_ALERTED_PREFIX}{}", o.id);
            let already = db::settings::get(conn, &alerted_key)?.is_some();
            if wedged && !already {
                actions.push(WedgeAction {
                    id: o.id.clone(),
                    name: o.name.clone(),
                    age_min: age / 60,
                    cpu_pct: o.cpu_pct.unwrap_or(0.0),
                    streak,
                });
            }
        }

        // Cleanup: drop streak + dedup rows for containers no longer present.
        for (key, _) in db::settings::list_prefix(conn, WEDGE_STREAK_PREFIX)? {
            let id = &key[WEDGE_STREAK_PREFIX.len()..];
            if !present.contains(id)
                && let Err(e) = db::settings::delete(conn, &key)
            {
                tracing::debug!("[maintenance] runner_wedge: streak cleanup {key}: {e:#}");
            }
        }
        for (key, _) in db::settings::list_prefix(conn, WEDGE_ALERTED_PREFIX)? {
            let id = &key[WEDGE_ALERTED_PREFIX.len()..];
            if !present.contains(id)
                && let Err(e) = db::settings::delete(conn, &key)
            {
                tracing::debug!("[maintenance] runner_wedge: dedup cleanup {key}: {e:#}");
            }
        }
        Ok((policy, actions))
    })
    .unwrap_or_else(|e| {
        tracing::debug!("[maintenance] runner_wedge: state pass failed: {e:#}");
        (remediation::RemediationPolicy::default(), Vec::new())
    });

    // Guard 3: policy gate. Disabled ⇒ neither act nor notify (silent).
    for action in &actions {
        let killed = if policy.acts() {
            docker_kill(&action.id).await
        } else {
            false
        };
        if policy.notifies() || killed {
            emit_wedge_event(action, killed).await;
        }
        // Dedup marker only once we acted or notified; Disabled leaves no marker
        // so a later policy change can still act on the same wedge.
        if killed || policy.notifies() {
            let alerted_key = format!("{WEDGE_ALERTED_PREFIX}{}", action.id);
            let r = db::pool::with_pooled_or_open(|conn| {
                db::settings::set(conn, &alerted_key, "1")?;
                Ok(())
            });
            if let Err(e) = r {
                tracing::debug!("[maintenance] runner_wedge: dedup persist failed: {e:#}");
            }
        }
    }
    Ok(())
}

/// Enumerate prefix-matched candidate containers and attach an age + CPU reading
/// to each. All docker calls are bounded; failures degrade to empty/None.
async fn observe_wedge_candidates(cfg: &WedgeConfig) -> Vec<WedgeObserved> {
    // `docker ps` — id, name, created-at. `--filter name=` is substring, so we
    // STILL re-check the anchored prefix in code below.
    let ps = docker_output(&[
        "ps",
        "--no-trunc",
        "--filter",
        &format!("name={}", cfg.prefix),
        "--format",
        "{{.ID}}\t{{.Names}}\t{{.CreatedAt}}",
    ])
    .await;
    let mut candidates: Vec<(String, String, String)> = Vec::new();
    for line in ps.lines() {
        let mut cols = line.split('\t');
        let (Some(id), Some(name), created) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        if matches_runner_prefix(name, &cfg.prefix) {
            candidates.push((
                id.to_string(),
                name.to_string(),
                created.unwrap_or("").to_string(),
            ));
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    // One CPU snapshot across all containers; match rows to candidates by
    // name equality or short-id prefix of the full id.
    let stats = docker_output(&[
        "stats",
        "--no-stream",
        "--format",
        "{{.Container}}\t{{.CPUPerc}}",
    ])
    .await;
    let cpu_rows: Vec<(String, f64)> = stats
        .lines()
        .filter_map(|l| {
            let mut c = l.split('\t');
            let token = c.next()?.trim().to_string();
            let pct = parse_cpu_pct(c.next()?)?;
            Some((token, pct))
        })
        .collect();

    let mut observed = Vec::new();
    for (id, name, created) in candidates {
        // Age: prefer inspect StartedAt (RFC3339), fall back to ps CreatedAt.
        let started_raw =
            docker_output(&["inspect", "--format", "{{.State.StartedAt}}", &id]).await;
        let started_unix =
            parse_container_start(started_raw.trim()).or_else(|| parse_container_start(&created));
        let cpu_pct = cpu_rows
            .iter()
            .find(|(token, _)| *token == name || id.starts_with(token.as_str()))
            .map(|(_, pct)| *pct);
        observed.push(WedgeObserved {
            id,
            name,
            started_unix,
            cpu_pct,
        });
    }
    observed
}

/// Run a bounded, unprivileged `docker` command, returning stdout (empty on any
/// failure/timeout). Never panics.
async fn docker_output(args: &[&str]) -> String {
    let fut = tokio::process::Command::new("docker").args(args).output();
    match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(Ok(o)) => {
            tracing::debug!(
                "[maintenance] runner_wedge docker {:?} exited {}: {}",
                args,
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            String::new()
        }
        Ok(Err(e)) => {
            tracing::debug!("[maintenance] runner_wedge docker {args:?} spawn: {e:#}");
            String::new()
        }
        Err(_) => {
            tracing::debug!("[maintenance] runner_wedge docker {args:?} timed out");
            String::new()
        }
    }
}

/// `docker kill <id>` (bounded). Returns whether the kill succeeded.
async fn docker_kill(id: &str) -> bool {
    let fut = tokio::process::Command::new("docker")
        .args(["kill", id])
        .output();
    match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Ok(Ok(o)) if o.status.success() => {
            tracing::warn!("[maintenance] runner_wedge: killed wedged container {id}");
            true
        }
        Ok(Ok(o)) => {
            tracing::warn!(
                "[maintenance] runner_wedge: docker kill {id} exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Ok(Err(e)) => {
            tracing::warn!("[maintenance] runner_wedge: docker kill {id} spawn: {e:#}");
            false
        }
        Err(_) => {
            tracing::warn!("[maintenance] runner_wedge: docker kill {id} timed out");
            false
        }
    }
}

/// Emit the wedge evidence event (best-effort fan-out).
async fn emit_wedge_event(action: &WedgeAction, killed: bool) {
    let host = sysinfo::System::host_name().unwrap_or_else(|| "this host".to_string());
    let verb = if killed { "killed" } else { "flagged" };
    let title = format!("wedged gitea runner container {verb} on {host}");
    let body = format!(
        "Container {} on {host} has been Up ~{} min with observed CPU {:.2}% across {} consecutive idle check(s) — the frozen-job-holding-a-runner-slot signature. Action: {}.",
        action.name,
        action.age_min,
        action.cpu_pct,
        action.streak,
        if killed {
            "killed (docker kill)"
        } else {
            "flagged only (policy did not authorize kill)"
        }
    );
    let event = Event::new(
        EventClass::Alert,
        Severity::Warn,
        title,
        "system.maintenance.runner_wedge",
    )
    .with_host(host)
    .with_body(body);
    let _ = notifications::emit(&event).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with_settings() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn threshold_returns_default_when_unset() {
        let conn = conn_with_settings();
        assert_eq!(
            threshold(&conn, "db.maintenance.warn_bytes", DEFAULT_WARN_BYTES),
            DEFAULT_WARN_BYTES
        );
    }

    #[test]
    fn threshold_reads_override_when_present() {
        let conn = conn_with_settings();
        db::settings::set(&conn, "db.maintenance.max_bytes", "12345").unwrap();
        assert_eq!(
            threshold(&conn, "db.maintenance.max_bytes", DEFAULT_MAX_BYTES),
            12345
        );
    }

    #[test]
    fn threshold_trims_whitespace() {
        let conn = conn_with_settings();
        db::settings::set(&conn, "k", "  777  ").unwrap();
        assert_eq!(threshold(&conn, "k", 1), 777);
    }

    #[test]
    fn threshold_falls_back_on_unparseable_value() {
        let conn = conn_with_settings();
        db::settings::set(&conn, "k", "not-a-number").unwrap();
        assert_eq!(
            threshold(&conn, "k", DEFAULT_MIN_VACUUM_BYTES),
            DEFAULT_MIN_VACUUM_BYTES
        );
    }

    #[test]
    fn threshold_accepts_negative_override() {
        let conn = conn_with_settings();
        db::settings::set(&conn, "k", "-5").unwrap();
        assert_eq!(threshold(&conn, "k", 100), -5);
    }

    #[test]
    fn should_install_baseline_only_when_absent() {
        assert!(should_install_baseline(false));
        assert!(!should_install_baseline(true));
    }

    #[test]
    fn level_for_boundaries() {
        // Default bands: <85 Ok, [85,95) Warn, >=95 Critical.
        assert_eq!(level_for(0, 85, 95), DiskLevel::Ok);
        assert_eq!(level_for(84, 85, 95), DiskLevel::Ok);
        assert_eq!(level_for(85, 85, 95), DiskLevel::Warn);
        assert_eq!(level_for(94, 85, 95), DiskLevel::Warn);
        assert_eq!(level_for(95, 85, 95), DiskLevel::Critical);
        assert_eq!(level_for(100, 85, 95), DiskLevel::Critical);
    }

    #[test]
    fn should_emit_on_increase() {
        assert!(should_emit(DiskLevel::Ok, DiskLevel::Warn));
        assert!(should_emit(DiskLevel::Warn, DiskLevel::Critical));
        assert!(should_emit(DiskLevel::Ok, DiskLevel::Critical));
    }

    #[test]
    fn should_emit_on_recovery_to_ok() {
        assert!(should_emit(DiskLevel::Warn, DiskLevel::Ok));
        assert!(should_emit(DiskLevel::Critical, DiskLevel::Ok));
    }

    #[test]
    fn should_not_emit_when_unchanged() {
        assert!(!should_emit(DiskLevel::Ok, DiskLevel::Ok));
        assert!(!should_emit(DiskLevel::Warn, DiskLevel::Warn));
        assert!(!should_emit(DiskLevel::Critical, DiskLevel::Critical));
    }

    #[test]
    fn should_not_emit_on_partial_drop() {
        // Crit→Warn is still an alerting state; don't re-notify until it either
        // climbs back to Crit or fully recovers to Ok.
        assert!(!should_emit(DiskLevel::Critical, DiskLevel::Warn));
    }

    #[test]
    fn disk_level_token_roundtrips() {
        for l in [DiskLevel::Ok, DiskLevel::Warn, DiskLevel::Critical] {
            assert_eq!(DiskLevel::parse(l.as_token()), l);
        }
        assert_eq!(DiskLevel::parse(""), DiskLevel::Ok);
        assert_eq!(DiskLevel::parse("bogus"), DiskLevel::Ok);
    }

    #[test]
    fn parse_df_root_reads_capacity_and_avail() {
        // Realistic `df -P /` output: header + one rootfs data row at 60%.
        let out = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                   /dev/rootfs       10485760  6291456   4194304      60% /\n";
        let u = parse_df_root(out).expect("should parse");
        assert_eq!(u.used_pct, 60);
        assert_eq!(u.mount, "/");
        assert_eq!(u.total_gb, 10); // 10485760 KiB = 10 GiB
        assert_eq!(u.avail_gb, 4); // 4194304 KiB = 4 GiB
    }

    #[test]
    fn parse_df_root_rejects_header_only_and_garbage() {
        assert!(
            parse_df_root("Filesystem 1024-blocks Used Available Capacity Mounted on\n").is_none()
        );
        assert!(parse_df_root("").is_none());
        assert!(parse_df_root("not a df table at all\n").is_none());
    }

    #[test]
    fn guest_disk_key_namespaces_by_vmid() {
        assert_eq!(guest_disk_key("100", "/"), "disk_alert_level:guest:100:/");
    }

    fn entry(path: &str, size: u64, mtime_unix: u64) -> TrimEntry {
        TrimEntry {
            path: PathBuf::from(path),
            size,
            mtime_unix,
        }
    }

    #[test]
    fn total_size_sums_entries() {
        let entries = [
            entry("/var/cache/example/a", 100, 1),
            entry("/var/cache/example/b", 250, 2),
            entry("/var/cache/example/c", 50, 3),
        ];
        assert_eq!(total_size(&entries), 400);
    }

    #[test]
    fn plan_trim_empty_when_under_or_at_cap() {
        let entries = [
            entry("/var/cache/example/a", 100, 1),
            entry("/var/cache/example/b", 100, 2),
        ];
        // Under budget.
        assert!(plan_trim(&entries, 500).is_empty());
        // Exactly at cap ⇒ nothing to delete.
        assert!(plan_trim(&entries, 200).is_empty());
    }

    #[test]
    fn plan_trim_deletes_oldest_first_until_under_cap() {
        // Ages ascending by mtime: a(oldest) < b < c(newest). Total 300, cap 150
        // ⇒ delete a (→200) then b (→100 ≤ 150), stop before c.
        let entries = [
            entry("/var/cache/example/c", 100, 30),
            entry("/var/cache/example/a", 100, 10),
            entry("/var/cache/example/b", 100, 20),
        ];
        let doomed = plan_trim(&entries, 150);
        assert_eq!(
            doomed,
            vec![
                PathBuf::from("/var/cache/example/a"),
                PathBuf::from("/var/cache/example/b"),
            ]
        );
    }

    #[test]
    fn plan_trim_deletes_nothing_more_than_necessary() {
        // Total 300, cap 250 ⇒ deleting the single oldest (100) brings it to 200.
        let entries = [
            entry("/var/cache/example/a", 100, 1),
            entry("/var/cache/example/b", 100, 2),
            entry("/var/cache/example/c", 100, 3),
        ];
        let doomed = plan_trim(&entries, 250);
        assert_eq!(doomed, vec![PathBuf::from("/var/cache/example/a")]);
    }

    #[test]
    fn cache_level_bands() {
        let cap = 10 * 1024 * 1024 * 1024; // 10 GiB
        // At/under cap ⇒ Ok.
        assert_eq!(cache_level(0, cap), DiskLevel::Ok);
        assert_eq!(cache_level(cap, cap), DiskLevel::Ok);
        // Between cap and 2×cap ⇒ Warn.
        assert_eq!(cache_level(cap + 1, cap), DiskLevel::Warn);
        assert_eq!(cache_level(2 * cap, cap), DiskLevel::Warn);
        // Over 2×cap ⇒ Critical.
        assert_eq!(cache_level(2 * cap + 1, cap), DiskLevel::Critical);
    }

    #[test]
    fn cache_level_guards_zero_cap() {
        // No divide-by-zero: empty is Ok, anything non-empty is Critical.
        assert_eq!(cache_level(0, 0), DiskLevel::Ok);
        assert_eq!(cache_level(1, 0), DiskLevel::Critical);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn db_size_pass_defaults_are_ordered() {
        // Sanity on the compiled thresholds the pass relies on: warn < max,
        // and the vacuum floor sits below both.
        assert!(DEFAULT_WARN_BYTES < DEFAULT_MAX_BYTES);
        assert!(DEFAULT_MIN_VACUUM_BYTES < DEFAULT_WARN_BYTES);
        assert!(FREE_RATIO_TRIGGER > 0.0 && FREE_RATIO_TRIGGER < 1.0);
    }

    #[test]
    fn db_size_pass_default_has_no_warnings() {
        let pass = DbSizePass::default();
        assert!(pass.warnings.is_empty());
    }

    // ── End-to-end passes over a real (unencrypted, temp) database. ──
    //
    // `with_pooled_or_open` finds no process pool in tests and falls through to
    // `open_default()`, which honors `$ORCA_DB_PATH`. Pointing it at a fresh
    // temp file gives each pass a fully-migrated database to work on. The env
    // var is process-global; nextest isolates each test in its own process, and
    // the mutex serializes the fallback under a threaded `cargo test` harness.
    use std::sync::Mutex;
    static DB_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Point `ORCA_DB_PATH` at a fresh temp db and run `f`, holding the env lock
    /// for the duration so concurrent tests don't clobber each other's path.
    fn with_temp_db<R>(f: impl FnOnce() -> R) -> R {
        let _guard = DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orca.db");
        let prev = std::env::var_os("ORCA_DB_PATH");
        unsafe { std::env::set_var("ORCA_DB_PATH", &path) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ORCA_DB_PATH", v),
                None => std::env::remove_var("ORCA_DB_PATH"),
            }
        }
        out
    }

    #[test]
    fn db_size_pass_on_fresh_db_stays_under_warn_threshold() {
        // A freshly-migrated database is a few pages — well under the 100 MiB
        // default warn threshold, so the pass completes with no warnings.
        let pass = with_temp_db(db_size_pass);
        assert!(
            pass.warnings.is_empty(),
            "fresh db should not warn: {:?}",
            pass.warnings.iter().map(|e| &e.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn db_size_pass_warns_when_over_configured_warn_bytes() {
        // Drop the warn threshold to 1 byte via the settings override so the
        // (small, non-empty) fresh db trips the size warning. Exercises the
        // warn-Event construction path.
        let pass = with_temp_db(|| {
            db::pool::with_pooled_or_open(|conn| {
                db::settings::set(conn, "db.maintenance.warn_bytes", "1").unwrap();
                Ok(())
            })
            .unwrap();
            db_size_pass()
        });
        assert_eq!(pass.warnings.len(), 1, "expected exactly one size warning");
        let ev = &pass.warnings[0];
        assert_eq!(ev.source, "system.maintenance.db_size");
        assert!(
            ev.title.contains("orca.db is"),
            "unexpected title: {}",
            ev.title
        );
        assert!(matches!(ev.severity, Severity::Warn));
    }

    /// Async variant of [`with_temp_db`]: holds the env lock across the awaited
    /// future so the tick sees a stable `ORCA_DB_PATH`.
    // Serializing env access requires holding the std mutex across the awaited
    // future; that is the point of the guard, so silence the lock-across-await
    // lint for this test-only helper.
    #[allow(clippy::await_holding_lock)]
    async fn with_temp_db_async<F, Fut, R>(f: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let _guard = DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orca.db");
        let prev = std::env::var_os("ORCA_DB_PATH");
        unsafe { std::env::set_var("ORCA_DB_PATH", &path) };
        let out = f().await;
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ORCA_DB_PATH", v),
                None => std::env::remove_var("ORCA_DB_PATH"),
            }
        }
        out
    }

    #[tokio::test]
    async fn sweep_tick_completes_on_migrated_db() {
        // Drives the full retention sweep (session events, pod offers,
        // replication-op reap) against a real migrated db; every table exists,
        // so the pass must complete without error.
        let res = with_temp_db_async(sweep_tick).await;
        assert!(res.is_ok(), "sweep_tick failed: {res:?}");
    }

    #[tokio::test]
    async fn db_size_tick_completes_on_migrated_db() {
        // Lower the warn threshold first so the tick also walks the
        // notification fan-out branch, then confirm the tick returns Ok.
        let res = with_temp_db_async(|| async {
            db::pool::with_pooled_or_open(|conn| {
                db::settings::set(conn, "db.maintenance.warn_bytes", "1").unwrap();
                Ok(())
            })
            .unwrap();
            db_size_tick().await
        })
        .await;
        assert!(res.is_ok(), "db_size_tick failed: {res:?}");
    }

    // ── Runner-wedge pure detection ──────────────────────────────────────────

    #[test]
    fn parse_cpu_pct_parses_and_rejects() {
        assert_eq!(parse_cpu_pct("0.00%"), Some(0.0));
        assert_eq!(parse_cpu_pct("12.50%"), Some(12.5));
        assert_eq!(parse_cpu_pct("  3.00% "), Some(3.0));
        assert_eq!(parse_cpu_pct("nan-percent"), None);
        assert_eq!(parse_cpu_pct("12.5"), None); // no % suffix
        assert_eq!(parse_cpu_pct(""), None);
    }

    #[test]
    fn next_streak_extends_below_and_resets_at_or_above() {
        assert_eq!(next_streak(2, 0.0, 1.0), 3); // below floor → +1
        assert_eq!(next_streak(2, 0.5, 1.0), 3);
        assert_eq!(next_streak(2, 1.0, 1.0), 0); // at floor → reset
        assert_eq!(next_streak(5, 9.0, 1.0), 0); // above floor → reset
    }

    #[test]
    fn is_wedged_requires_age_and_streak() {
        // both satisfied
        assert!(is_wedged(1801, 1800, 2, 2));
        // age boundary: equal is NOT wedged
        assert!(!is_wedged(1800, 1800, 5, 2));
        // streak boundary: stall-1 is NOT wedged
        assert!(!is_wedged(3600, 1800, 1, 2));
        // streak exactly at stall is wedged
        assert!(is_wedged(3600, 1800, 2, 2));
    }

    #[test]
    fn matches_runner_prefix_startswith_and_blank_matches_nothing() {
        let p = "GITEA-ACTIONS-TASK-";
        assert!(matches_runner_prefix(
            "GITEA-ACTIONS-TASK-1-WORKFLOW-ci-JOB-x-abc",
            p
        ));
        assert!(!matches_runner_prefix("some-other-container", p));
        // Blank / whitespace prefix must match NOTHING (never everything).
        assert!(!matches_runner_prefix("GITEA-ACTIONS-TASK-1", ""));
        assert!(!matches_runner_prefix("anything", "   "));
    }

    #[test]
    fn age_secs_saturates_non_negative() {
        assert_eq!(age_secs(1000, 2800), 1800);
        assert_eq!(age_secs(2800, 1000), 0); // clock skew → never negative
        assert_eq!(age_secs(1000, 1000), 0);
    }

    #[test]
    fn parse_container_start_handles_rfc3339_and_created_at() {
        // RFC3339 (docker inspect .State.StartedAt)
        let a = parse_container_start("2026-09-14T12:00:00Z").unwrap();
        // docker ps .CreatedAt form
        let b = parse_container_start("2026-09-14 12:00:00 +0000 UTC").unwrap();
        assert_eq!(a, b);
        assert_eq!(parse_container_start("not-a-time"), None);
    }

    #[test]
    fn is_truthy_matrix() {
        for v in ["1", "true", "TRUE", "yes", "on", "  On "] {
            assert!(is_truthy(v), "{v} should be truthy");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!is_truthy(v), "{v} should be falsey");
        }
    }
}
