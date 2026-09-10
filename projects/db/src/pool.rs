//! The data-access seam.
//!
//! ## Why a seam
//!
//! Historically every server DB call site named the MECHANISM directly —
//! `db::pool::with_pooled_or_open(|conn| …)` over a single
//! `Arc<Mutex<Connection>>`. 116 sites were coupled to *how* a connection is
//! acquired, so any concurrency change (a reader pool, `spawn_blocking`, a
//! BUSY-retry, tracing) had to touch all of them. That coupling is the real
//! problem behind the measured concurrency collapse (concurrent-100 `pod.list`
//! = 8.64s vs sequential 3.47s — 2.5× WORSE — every read fighting one mutex).
//!
//! [`Db`] is the fix: a single handle every call site goes through, exposing
//! **intent** — `read` / `write` / `tx` (+ `*_async`) — never mechanism.
//! EVERYTHING about strategy lives behind it: the writer mutex, the reader
//! pool, `spawn_blocking`, BUSY retry, the task-local path override, and (later)
//! metrics/tracing. A future rework is a change *here*, not at the call sites.
//!
//! ## The first strategy behind the seam
//!
//! SQLite is single-writer, so **all writes serialize through one writer
//! connection**. Reads go to a **pre-keyed reader pool** of N `query_only`
//! connections opened once at [`Db::init_process`]. Under the DELETE rollback
//! journal (NEVER WAL — SQLCipher + WAL multi-connection short-reads the -shm,
//! error 522; see `lib::apply_tuning_pragmas`) multiple connections hold SHARED
//! read locks concurrently, so reads genuinely parallelize; a committing writer
//! only briefly blocks them. `query_only = ON` on readers is a hard guard: a
//! write misrouted onto a reader is rejected by SQLite, not silently applied.
//!
//! The writer keeps the 64 MiB page cache; readers are capped to 8 MiB each
//! ([`READER_CACHE_KIB`]) so N readers don't inflate RSS by N×64 MiB.
//!
//! ## Off the async workers
//!
//! `read`/`write`/`tx` run the closure on the CALLER's thread. The `*_async`
//! variants wrap it in `tokio::task::spawn_blocking`, so a blocking SQLCipher
//! query (or the writer's `synchronous=FULL` fsync) never parks a tokio worker
//! and stalls unrelated async work.
//!
//! ## Legacy shims
//!
//! `with_pooled_or_open` (writer) and `with_read_pooled_or_open` (reader) remain
//! as thin shims that delegate into [`Db::process`], so CLI/test paths and
//! not-yet-migrated call sites keep working. They are the migration surface —
//! new code uses [`Db`] directly.

use crate::open_default;
use anyhow::Result;
use rusqlite::{Connection, Transaction};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

static PROCESS: OnceLock<Db> = OnceLock::new();

/// Per-reader page cache, in KiB (negative `cache_size` = KiB). 8 MiB keeps the
/// pool's total cache footprint bounded (~N×8 MiB) instead of N×64 MiB.
const READER_CACHE_KIB: i64 = 8192;

/// Upper bound on reader connections. Reads are short; a handful of connections
/// saturates the shared-lock parallelism the DELETE journal allows without
/// paying N× the open/KDF cost or cache RSS.
const MAX_READERS: usize = 8;

// ── The seam ───────────────────────────────────────────────────────────────

/// The single data-access handle. Cheap to clone (one `Arc`). All call sites go
/// through a `Db` and express intent (`read` / `write` / `tx`), never the
/// acquisition mechanism.
#[derive(Clone)]
pub struct Db {
    strategy: Arc<Strategy>,
}

/// The connection-acquisition strategy behind the seam. v1 ships two:
///   * `Pooled` — process default after [`Db::init_process`]: writer + reader pool.
///   * `PerCall` — CLI/test/pre-init: open a fresh connection per call, honoring
///     the task-local / thread-local / `ORCA_DB_PATH` path override in
///     `open_default`.
enum Strategy {
    Pooled(Pooled),
    PerCall,
}

/// Writer + reader pool. The writer is single (SQLite is single-writer); the
/// readers parallelize SELECT-only work.
struct Pooled {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
}

impl Db {
    /// The process-wide handle: the pooled strategy once [`Db::init_process`]
    /// has run, else a per-call strategy (CLI, tests, pre-startup).
    pub fn process() -> Db {
        PROCESS.get().cloned().unwrap_or_else(Db::per_call)
    }

    /// A per-call handle that opens a fresh `open_default()` connection for each
    /// `read`/`write`/`tx`. Honors the task-local/thread-local/env path
    /// override. Used by CLI (one call per process) and tests.
    pub fn per_call() -> Db {
        Db {
            strategy: Arc::new(Strategy::PerCall),
        }
    }

    /// Open the process-wide pooled strategy. Call once at server startup.
    /// Idempotent: subsequent calls return the existing handle unchanged.
    ///
    /// The writer opens FIRST (running `apply_schema` + migrations via
    /// `open_default`), so the readers that follow find the schema initialized
    /// and skip it — and, being `query_only`, could not create it anyway.
    pub fn init_process() -> Result<Db> {
        if let Some(db) = PROCESS.get() {
            return Ok(db.clone());
        }
        let writer = open_default()?;
        let n = reader_count();
        let mut readers = Vec::with_capacity(n);
        for _ in 0..n {
            readers.push(Mutex::new(open_reader()?));
        }
        let db = Db {
            strategy: Arc::new(Strategy::Pooled(Pooled {
                writer: Mutex::new(writer),
                readers,
                next_reader: AtomicUsize::new(0),
            })),
        };
        // Race-safe: if another thread won, drop ours and use theirs.
        match PROCESS.set(db.clone()) {
            Ok(()) => Ok(db),
            Err(_) => Ok(PROCESS.get().expect("just-set").clone()),
        }
    }

    /// Whether the process-wide pooled strategy has been initialized.
    pub fn is_process_initialized() -> bool {
        PROCESS.get().is_some()
    }

    // ── Intent: READ ──────────────────────────────────────────────────────

    /// Run a SELECT-only closure against a reader. On the pooled strategy this
    /// is a `query_only` pool connection (reads parallelize); on the per-call
    /// strategy it is a fresh open. Runs on the CALLER's thread.
    pub fn read<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        match &*self.strategy {
            Strategy::Pooled(p) => p.with_reader(f),
            Strategy::PerCall => f(&open_default()?),
        }
    }

    /// `read` on a `spawn_blocking` thread — keeps SQLCipher work off the tokio
    /// workers and parallelizes across reader connections. `f`/`R` must be
    /// `Send + 'static`.
    pub async fn read_async<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.read(f))
            .await
            .map_err(|e| anyhow::anyhow!("db read task join error: {e}"))?
    }

    // ── Intent: WRITE ─────────────────────────────────────────────────────

    /// Run a closure against the single writer connection (INSERT/UPDATE/DELETE/
    /// CREATE, or a read that must observe an in-flight write). Runs on the
    /// CALLER's thread.
    ///
    /// Statement-level lock contention is absorbed by `busy_timeout=5000ms` set
    /// on every connection; a closure-level SQLITE_BUSY retry (which requires
    /// idempotent, re-runnable closures — an `Fn` bound) is a future strategy
    /// concern the seam can add without touching call sites.
    pub fn write<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        match &*self.strategy {
            Strategy::Pooled(p) => p.with_writer(f),
            Strategy::PerCall => f(&open_default()?),
        }
    }

    /// `write` on a `spawn_blocking` thread — keeps the writer mutex +
    /// `synchronous=FULL` fsync off the tokio workers. `f`/`R` must be
    /// `Send + 'static`.
    pub async fn write_async<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.write(f))
            .await
            .map_err(|e| anyhow::anyhow!("db write task join error: {e}"))?
    }

    // ── Intent: TRANSACTION ───────────────────────────────────────────────

    /// Run a closure inside an explicit transaction on the writer. Commits on
    /// `Ok`, rolls back on `Err` (via `Transaction` drop). Multi-statement
    /// writes that must be atomic use this rather than a bare `write`.
    pub fn tx<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Transaction) -> Result<R>,
    {
        match &*self.strategy {
            Strategy::Pooled(p) => p.with_writer_tx(f),
            Strategy::PerCall => {
                let mut conn = open_default()?;
                let tx = conn.transaction()?;
                let r = f(&tx)?;
                tx.commit()?;
                Ok(r)
            }
        }
    }
}

impl Pooled {
    fn with_reader<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        // Round-robin the slot so callers spread across connections.
        let i = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        let guard = self.readers[i]
            .lock()
            .map_err(|e| anyhow::anyhow!("db reader pool poisoned: {e}"))?;
        f(&guard)
    }

    fn with_writer<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        let guard = self
            .writer
            .lock()
            .map_err(|e| anyhow::anyhow!("db writer poisoned: {e}"))?;
        f(&guard)
    }

    fn with_writer_tx<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Transaction) -> Result<R>,
    {
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| anyhow::anyhow!("db writer poisoned: {e}"))?;
        let tx = guard.transaction()?;
        let r = f(&tx)?;
        tx.commit()?;
        Ok(r)
    }
}

/// How many reader connections to open: `min(MAX_READERS, parallelism)`, min 2.
fn reader_count() -> usize {
    let par = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    par.clamp(2, MAX_READERS)
}

/// Open one reader connection: standard keyed + pragma'd open, then shrink the
/// page cache and mark it read-only. `query_only = ON` guarantees a write
/// routed onto a reader is rejected by SQLite rather than silently mutating
/// state — the safety net behind the read/write classification.
fn open_reader() -> Result<Connection> {
    let conn = open_default()?;
    conn.execute_batch(&format!(
        "PRAGMA cache_size = -{READER_CACHE_KIB}; PRAGMA query_only = ON;"
    ))?;
    Ok(conn)
}

// ── Legacy shims (delegate into the seam) ────────────────────────────────────

/// LEGACY. Prefer `Db::process().write(f)`. Kept as a thin shim so CLI/test
/// paths and not-yet-migrated call sites keep working. Routes to the WRITER —
/// the safe default for unclassified / mixed read+write sites; never puts a
/// write on a reader.
pub fn with_pooled_or_open<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&Connection) -> Result<R>,
{
    Db::process().write(f)
}

/// LEGACY. Prefer `Db::process().read(f)`. Thin shim routing a SELECT-only
/// closure to the reader pool. Use ONLY for closures that exclusively read; if
/// in any doubt use [`with_pooled_or_open`] (writer).
pub fn with_read_pooled_or_open<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&Connection) -> Result<R>,
{
    Db::process().read(f)
}

/// LEGACY name kept for the startup call site. Delegates to [`Db::init_process`].
pub struct DbPool;
impl DbPool {
    /// Initialize the process-wide pooled strategy. See [`Db::init_process`].
    pub fn init_or_get() -> Result<Db> {
        Db::init_process()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the seam against a throwaway unencrypted DB via a per-call handle
    /// (honors the thread-local path override; no process-global state touched).
    fn with_temp_db<R>(f: impl FnOnce(&Db) -> R) -> R {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        crate::with_thread_db_path(tmp.path(), || {
            let db = Db::per_call();
            db.write(|c| {
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS seam_kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);",
                )?;
                Ok(())
            })
            .unwrap();
            f(&db)
        })
    }

    #[test]
    fn write_then_read_round_trips() {
        with_temp_db(|db| {
            db.write(|c| {
                c.execute("INSERT INTO seam_kv (k, v) VALUES ('a', '1')", [])?;
                Ok(())
            })
            .unwrap();
            let v: String = db
                .read(|c| Ok(c.query_row("SELECT v FROM seam_kv WHERE k='a'", [], |r| r.get(0))?))
                .unwrap();
            assert_eq!(v, "1");
        });
    }

    #[test]
    fn tx_commits_on_ok_and_rolls_back_on_err() {
        with_temp_db(|db| {
            db.tx(|tx| {
                tx.execute("INSERT INTO seam_kv (k, v) VALUES ('c', '1')", [])?;
                Ok(())
            })
            .unwrap();
            // Rolls back: the closure errors after an insert, so nothing sticks.
            let rolled: Result<()> = db.tx(|tx| {
                tx.execute("INSERT INTO seam_kv (k, v) VALUES ('d', '2')", [])?;
                anyhow::bail!("boom")
            });
            assert!(rolled.is_err());
            let n: i64 = db
                .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM seam_kv", [], |r| r.get(0))?))
                .unwrap();
            assert_eq!(n, 1, "only the committed row 'c' should remain");
        });
    }

    #[tokio::test]
    async fn async_wrappers_run_off_thread() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        crate::with_db_path(tmp.path().to_path_buf(), async {
            let db = Db::per_call();
            db.write_async(|c| {
                c.execute_batch(
                    "CREATE TABLE seam_a (k TEXT PRIMARY KEY);
                     INSERT INTO seam_a VALUES ('x');",
                )?;
                Ok(())
            })
            .await
            .unwrap();
            let n: i64 = db
                .read_async(|c| Ok(c.query_row("SELECT COUNT(*) FROM seam_a", [], |r| r.get(0))?))
                .await
                .unwrap();
            assert_eq!(n, 1);
        })
        .await;
    }

    #[test]
    fn reader_count_is_bounded() {
        let n = reader_count();
        assert!((2..=MAX_READERS).contains(&n), "reader_count={n}");
    }
}
