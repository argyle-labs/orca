#!/usr/bin/env bash
# Dev mode: Rust + Vite on :12000 (Rust proxies non-API to Vite at :12001).
# Invoked via: op run --env-file .env.brain.tpl -- bash scripts/dev.sh

set -uo pipefail
set -m  # job control: each background job gets its own pgid (so we can signal whole trees)

BRAIN="$HOME/.local/bin/brain"
_CLEANUP_DONE=0

cleanup() {
  [[ $_CLEANUP_DONE -eq 1 ]] && return
  _CLEANUP_DONE=1
  echo ""
  echo "  stopping dev session..."
  # Ignore TERM in this shell so killing our own pgid doesn't re-enter cleanup.
  trap '' TERM
  # Signal every process in our process group — catches cargo-watch, the cargo-run
  # child it spawns, vite, npm, and the pipeline subshells that `jobs -p` misses.
  kill -TERM 0 2>/dev/null || true
  sleep 0.3
  kill -KILL 0 2>/dev/null || true
  "$BRAIN" daemon reclaim 2>/dev/null || true
}
trap 'cleanup; exit 0' INT TERM

# ── Refresh external rebuy specs ──────────────────────────────────────────────
# Done BEFORE the daemon-park handoff: the parked daemon auto-reclaims port
# 12000 every 5s if no dev session has registered yet, so anything between
# `daemon park` and `cargo watch` racing for >5s will lose the port.
echo "  syncing rebuy specs..."
"$BRAIN" spec sync --all 2>&1 | sed 's/^/[specs]    /' || true

# ── Hand off port 12000 to dev ────────────────────────────────────────────────
daemon_mode=$("$BRAIN" daemon status 2>/dev/null | awk '/mode:/ {print $2}' || echo "offline")

if [[ "$daemon_mode" == "running" ]]; then
  echo "  parking daemon..."
  "$BRAIN" daemon park 2>/dev/null && sleep 0.3 || {
    echo "  park failed — clearing port directly"
    while IFS= read -r pid; do kill "$pid" 2>/dev/null || true; done \
      < <(lsof -ti tcp:12000 2>/dev/null)
    sleep 0.3
  }
else
  # Daemon not in running mode (offline / parked / dev-superseded) — clear ports directly
  for port in 12000 12001; do
    while IFS= read -r pid; do
      echo "  clearing :$port (pid $pid)"
      kill "$pid" 2>/dev/null || true
    done < <(lsof -ti tcp:"$port" 2>/dev/null)
  done
  sleep 0.3
fi

# This script's PID stays alive across cargo-watch rebuilds — used by 'brain serve --dev'
# to register the dev session in state, preventing the daemon from auto-reclaiming the port.
export BRAIN_DEV_PARENT_PID=$$

echo ""
echo "  brain  →  http://localhost:12000  (rust + vite HMR)"
echo ""

# ── Start dev servers ─────────────────────────────────────────────────────────
BRAIN_LOG=trace cargo watch -q -c -C projects/server \
  -w src -w Cargo.toml \
  -x 'run -- serve --dev' 2>&1 | \
  while IFS= read -r line; do
    echo "[server]   $line"
    echo "$line" | grep -q "listening on" && \
      (sleep 0.5 && cd projects/frontend && npm run gen 2>&1 | sed 's/^/[gen]      /') &
  done &

(cd projects/frontend && npm run dev 2>&1 | sed 's/^/[frontend] /') &

wait
