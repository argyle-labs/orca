//! orca-db microbench — measures the impact of the Tier 1+2 SQLite tuning.
//!
//! Run with:
//!     cargo run --release -p orca-db --example db_bench --quiet
//!
//! The bench uses an unencrypted on-disk database in a temp directory so it
//! exercises the same WAL/mmap/page-cache code paths as production (in-memory
//! databases ignore most of those pragmas, which would skew the numbers).
//!
//! To capture a baseline before the optimizations:
//!   1. git stash the optimization commit
//!   2. cargo clean -p libsqlite3-sys   (force C-side rebuild without LIBSQLITE3_FLAGS)
//!   3. cargo run --release -p orca-db --example db_bench --quiet
//!   4. git stash pop && cargo clean -p libsqlite3-sys
//!   5. cargo run --release -p orca-db --example db_bench --quiet
//!   6. diff the two output blocks

use rusqlite::Connection;
use std::time::{Duration, Instant};

/// One synthetic workload.
struct Bench {
    name: &'static str,
    iters: u32,
    /// Run once before the iter loop. Should leave the connection in
    /// autocommit mode (no open transaction).
    setup: fn(&Connection),
    /// If true, harness wraps the iter loop in BEGIN ... COMMIT.
    transactional: bool,
    /// Per-iter work. The connection is shared across iters.
    work: fn(&Connection, u32),
}

fn main() {
    let benches: &[Bench] = &[
        Bench {
            name: "open + apply pragmas",
            iters: 50,
            transactional: false,
            setup: |_| {},
            work: |_, _| {
                // Each iter opens a fresh connection — measures the full
                // connection-establish + pragma-apply cost.
                let tmp = tempfile::NamedTempFile::new().unwrap();
                let _conn = db::open_unencrypted(tmp.path()).unwrap();
            },
        },
        Bench {
            name: "single insert (autocommit, fsync per row)",
            iters: 1_000,
            transactional: false,
            setup: |c| {
                c.execute_batch(
                    "DROP TABLE IF EXISTS bench_kv;
                     CREATE TABLE bench_kv (k INTEGER PRIMARY KEY, v TEXT NOT NULL);",
                )
                .unwrap();
            },
            work: |c, i| {
                c.execute(
                    "INSERT INTO bench_kv (k, v) VALUES (?1, ?2)",
                    rusqlite::params![i, format!("value-{i}")],
                )
                .unwrap();
            },
        },
        Bench {
            name: "single insert (batched in one tx)",
            iters: 50_000,
            transactional: true,
            setup: |c| {
                c.execute_batch(
                    "DROP TABLE IF EXISTS bench_kv2;
                     CREATE TABLE bench_kv2 (k INTEGER PRIMARY KEY, v TEXT NOT NULL);",
                )
                .unwrap();
            },
            work: |c, i| {
                c.execute(
                    "INSERT INTO bench_kv2 (k, v) VALUES (?1, ?2)",
                    rusqlite::params![i, format!("value-{i}")],
                )
                .unwrap();
            },
        },
        Bench {
            name: "select by primary key",
            iters: 50_000,
            transactional: false,
            setup: |c| {
                c.execute_batch(
                    "DROP TABLE IF EXISTS bench_pk;
                     CREATE TABLE bench_pk (k INTEGER PRIMARY KEY, v TEXT NOT NULL);
                     BEGIN;",
                )
                .unwrap();
                for i in 0..10_000 {
                    c.execute(
                        "INSERT INTO bench_pk (k, v) VALUES (?1, ?2)",
                        rusqlite::params![i, format!("value-{i}")],
                    )
                    .unwrap();
                }
                c.execute_batch("COMMIT;").unwrap();
            },
            work: |c, i| {
                let k = (i as i64) % 10_000;
                let _v: String = c
                    .query_row("SELECT v FROM bench_pk WHERE k = ?1", [k], |r| r.get(0))
                    .unwrap();
            },
        },
        Bench {
            name: "scan 1000 rows",
            iters: 200,
            transactional: false,
            setup: |_| {
                // Reuses bench_pk from the previous bench. No-op setup.
            },
            work: |c, _| {
                let mut stmt = c
                    .prepare_cached("SELECT k, v FROM bench_pk LIMIT 1000")
                    .unwrap();
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                    .unwrap();
                let mut n = 0;
                for r in rows {
                    let _ = r.unwrap();
                    n += 1;
                }
                assert_eq!(n, 1000);
            },
        },
        Bench {
            name: "json_extract on 1000 rows",
            iters: 200,
            transactional: false,
            setup: |c| {
                c.execute_batch(
                    "DROP TABLE IF EXISTS bench_json;
                     CREATE TABLE bench_json (k INTEGER PRIMARY KEY, j TEXT NOT NULL);
                     BEGIN;",
                )
                .unwrap();
                for i in 0..1000 {
                    let j = format!(r#"{{"id":{i},"name":"name-{i}","tags":[1,2,3]}}"#);
                    c.execute(
                        "INSERT INTO bench_json (k, j) VALUES (?1, ?2)",
                        rusqlite::params![i, j],
                    )
                    .unwrap();
                }
                c.execute_batch("COMMIT;").unwrap();
            },
            work: |c, _| {
                let mut stmt = c
                    .prepare_cached("SELECT json_extract(j, '$.name') FROM bench_json LIMIT 1000")
                    .unwrap();
                let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
                let mut n = 0;
                for r in rows {
                    let _ = r.unwrap();
                    n += 1;
                }
                assert_eq!(n, 1000);
            },
        },
    ];

    // Workloads share a single connection so the page cache + cached statements
    // carry over (mirrors how the orca server uses pooled connections).
    // Open is logged-noisy because of migrations — we want that ONCE, then we
    // print the bench results below it.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let shared = db::open_unencrypted(tmp.path()).unwrap();

    println!();
    println!("orca-db bench — release profile");
    println!(
        "{:42}  {:>8}  {:>10}  {:>10}  {:>10}  {:>10}",
        "workload", "iters", "p50 (µs)", "p95 (µs)", "p99 (µs)", "ops/sec"
    );
    println!("{}", "─".repeat(99));

    for b in benches {
        // Skip the shared-conn setup for "open + apply pragmas" — it makes its
        // own connections. For everything else, run the bench's setup against
        // the shared connection.
        if b.name != "open + apply pragmas" {
            (b.setup)(&shared);
        }

        if b.transactional {
            shared.execute_batch("BEGIN;").unwrap();
        }

        // Warmup: 5% of iters, not measured.
        let warmup = (b.iters / 20).max(1);
        for i in 0..warmup {
            (b.work)(&shared, i);
        }
        let mut samples = Vec::with_capacity(b.iters as usize);
        for i in 0..b.iters {
            let t = Instant::now();
            (b.work)(&shared, i + warmup);
            samples.push(t.elapsed());
        }

        if b.transactional {
            shared.execute_batch("COMMIT;").unwrap();
        }

        report(b.name, b.iters, &mut samples);
    }
}

fn report(name: &str, iters: u32, samples: &mut [Duration]) {
    samples.sort();
    let p = |q: f64| -> u64 {
        let idx = ((samples.len() as f64) * q).min(samples.len() as f64 - 1.0) as usize;
        samples[idx].as_micros() as u64
    };
    let total: Duration = samples.iter().sum();
    let ops_per_sec = (iters as f64) / total.as_secs_f64();
    println!(
        "{:42}  {:>8}  {:>10}  {:>10}  {:>10}  {:>10.0}",
        name,
        iters,
        p(0.50),
        p(0.95),
        p(0.99),
        ops_per_sec,
    );
}
