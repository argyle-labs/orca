#!/usr/bin/env bash
# Dev mode: Rust API on :12000 + Vite on :12001 with hot reload.
# Invoked via: op run --env-file .env.brain.tpl -- bash scripts/dev.sh

set -euo pipefail

for port in 12000 12001; do
  pid=$(lsof -ti tcp:"$port" 2>/dev/null || true)
  if [ -n "$pid" ]; then
    echo "  killing stale process on :$port (pid $pid)"
    kill -9 "$pid" 2>/dev/null || true
  fi
done

echo ""
echo "  brain       →  http://localhost:12000  (rust proxies → vite HMR)"
echo "  brain gen   →  runs after each backend restart"
echo ""

trap 'kill 0' SIGINT SIGTERM

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
