//! orca-db concurrency microbench — measures the reader-pool vs single-writer
//! read path under concurrent load. Directly exercises the root cause the
//! `perf/db-reader-pool-spawn-blocking` rework targets: concurrent reads that
//! used to serialize on one `Arc<Mutex<Connection>>`.
//!
//! Run with:
//!     ORCA_DB_PATH=$(mktemp -u).db \
//!       cargo run --release -p db --example db_concurrency_bench --quiet
//!
//! Uses an unencrypted on-disk DB (via `ORCA_DB_PATH`) so the seam's
//! `open_default()` path is exercised end to end. Each "read" is a CPU-bound
//! aggregate scan over a seeded table, so parallel readers across cores show a
//! real wall-clock difference from the serialized single-writer path — the same
//! shape as SQLCipher page decryption under the encrypted production DB.
//!
//! WRITER  column = every read routed through `Db::write` (the OLD read path:
//!                  single writer mutex, serialized).
//! READERS column = every read routed through `Db::read` (the NEW reader pool:
//!                  parallel across N `query_only` connections).

use db::pool::Db;
use std::time::Instant;

const SEED_ROWS: i64 = 40_000;
const READS_PER_THREAD: usize = 40;

fn seed(db: &Db) {
    db.write(|c| {
        c.execute_batch(
            "DROP TABLE IF EXISTS bench_scan;
             CREATE TABLE bench_scan (k INTEGER PRIMARY KEY, v INTEGER NOT NULL);
             BEGIN;",
        )?;
        {
            let mut stmt = c.prepare("INSERT INTO bench_scan (k, v) VALUES (?1, ?2)")?;
            for k in 0..SEED_ROWS {
                stmt.execute(rusqlite::params![k, k * 7 % 101])?;
            }
        }
        c.execute_batch("COMMIT;")?;
        Ok(())
    })
    .expect("seed");
}

/// One CPU-bound read: a full-table aggregate scan. Returns the aggregate so
/// the optimizer can't elide the work.
fn one_read(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT COALESCE(SUM(v), 0) FROM bench_scan WHERE v > 0",
        [],
        |r| r.get(0),
    )
    .expect("scan")
}

/// Which intent each read is routed through.
#[derive(Clone, Copy)]
enum Route {
    Writer,
    Readers,
}

/// Run `threads` OS threads, each issuing `READS_PER_THREAD` reads via `route`.
/// Returns (wall_ms, p50_ms, p95_ms, max_ms) over all per-call latencies.
fn run(db: &Db, threads: usize, route: Route) -> (f64, f64, f64, f64) {
    let start = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                let mut lat = Vec::with_capacity(READS_PER_THREAD);
                for _ in 0..READS_PER_THREAD {
                    let t = Instant::now();
                    let n = match route {
                        Route::Writer => db.write(|c| Ok(one_read(c))).unwrap(),
                        Route::Readers => db.read(|c| Ok(one_read(c))).unwrap(),
                    };
                    std::hint::black_box(n);
                    lat.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                lat
            })
        })
        .collect();
    let mut all: Vec<f64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let wall = start.elapsed().as_secs_f64() * 1000.0;
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| all[((all.len() as f64 * p) as usize).min(all.len() - 1)];
    (wall, pct(0.50), pct(0.95), all[all.len() - 1])
}

fn main() {
    // Initialize the process-wide pooled strategy against the ORCA_DB_PATH DB.
    let db = Db::init_process().expect("init pool");
    seed(&db);

    println!(
        "orca-db concurrency bench — {SEED_ROWS} rows, {READS_PER_THREAD} reads/thread\n\
         path={}\n",
        std::env::var("ORCA_DB_PATH").unwrap_or_else(|_| "<default>".into())
    );
    println!(
        "{:>8} | {:<7} | {:>10} {:>9} {:>9} {:>9}",
        "threads", "route", "wall_ms", "p50_ms", "p95_ms", "max_ms"
    );
    println!("{}", "-".repeat(64));

    for &n in &[1usize, 10, 50, 100, 200] {
        for (label, route) in [("writer", Route::Writer), ("readers", Route::Readers)] {
            let (wall, p50, p95, max) = run(&db, n, route);
            println!("{n:>8} | {label:<7} | {wall:>10.1} {p50:>9.2} {p95:>9.2} {max:>9.2}");
        }
    }
}
