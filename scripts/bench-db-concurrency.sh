#!/usr/bin/env bash
# bench-db-concurrency.sh — measure DB read concurrency before/after the
# reader-pool seam (perf/db-reader-pool-spawn-blocking).
#
# Two layers, pick with the first arg:
#
#   micro   (default) — runs the db-crate microbench
#                       (examples/db_concurrency_bench.rs) which routes the SAME
#                       concurrent read workload through BOTH the old single-
#                       writer path (`Db::write`) and the new reader pool
#                       (`Db::read`) in one process, printing wall + p50/p95/max
#                       per thread count {1,10,50,100,200}. This isolates the
#                       seam's effect with no daemon to stand up — the numbers in
#                       the PR come from here.
#
#   daemon  — end-to-end: builds the release `orca` binary, boots a throwaway
#             daemon on a test port against a temp DB, then times
#             `orca pod list` sequentially (100×) and concurrently
#             ({10,50,100,200}×). Requires a buildable release daemon and a free
#             port; use to confirm the micro result carries to the real surface.
#
# Usage:
#   scripts/bench-db-concurrency.sh micro
#   scripts/bench-db-concurrency.sh daemon [PORT]
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
MODE="${1:-micro}"

pctile() { # pctile <fraction> < sorted_numbers
  awk -v p="$1" '{a[NR]=$0} END{if(NR==0){print 0;exit} i=int(NR*p); if(i<1)i=1; if(i>NR)i=NR; print a[i]}'
}

run_micro() {
  echo "== db-crate microbench (writer path vs reader pool) =="
  cargo build --release -p db --example db_concurrency_bench
  ORCA_DB_PATH="$(mktemp -u)_bench.db" \
    ./target/release/examples/db_concurrency_bench
}

run_daemon() {
  local port="${1:-12777}"
  local db; db="$(mktemp -u)_bench.db"
  echo "== daemon end-to-end bench (orca pod list) on port $port =="
  cargo build --release --bin orca
  local ORCA=./target/release/orca

  ORCA_DB_PATH="$db" "$ORCA" daemon --port "$port" >/tmp/bench-daemon.log 2>&1 &
  local pid=$!
  trap 'kill $pid 2>/dev/null || true' EXIT
  # Wait for readiness.
  for _ in $(seq 1 50); do
    if ORCA_API="http://127.0.0.1:$port" "$ORCA" pod list >/dev/null 2>&1; then break; fi
    sleep 0.2
  done

  bench() { # bench <label> <concurrency> <total>
    local label="$1" conc="$2" total="$3" tmp; tmp="$(mktemp)"
    local start; start=$(python3 -c 'import time;print(time.time())')
    local i=0
    while [ "$i" -lt "$total" ]; do
      local batch=0
      while [ "$batch" -lt "$conc" ] && [ "$i" -lt "$total" ]; do
        { /usr/bin/time -p env ORCA_API="http://127.0.0.1:$port" "$ORCA" pod list >/dev/null 2>>"$tmp"; } &
        batch=$((batch+1)); i=$((i+1))
      done
      wait
    done
    local end; end=$(python3 -c 'import time;print(time.time())')
    local reals; reals="$(grep real "$tmp" | awk '{print $2}' | sort -n)"
    printf '%-16s conc=%-4s total=%-4s wall=%.2fs p50=%ss p95=%ss max=%ss\n' \
      "$label" "$conc" "$total" \
      "$(python3 -c "print($end-$start)")" \
      "$(echo "$reals" | pctile 0.50)" \
      "$(echo "$reals" | pctile 0.95)" \
      "$(echo "$reals" | tail -1)"
    rm -f "$tmp"
  }

  bench "sequential-100" 1 100
  for c in 10 50 100 200; do bench "concurrent-$c" "$c" "$c"; done
}

case "$MODE" in
  micro)  run_micro ;;
  daemon) run_daemon "${2:-}" ;;
  *) echo "usage: $0 {micro|daemon [port]}" >&2; exit 2 ;;
esac
