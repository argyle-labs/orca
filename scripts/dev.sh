#!/usr/bin/env bash
# Dev mode: Rust + Vite on :12000 (Rust proxies non-API to Vite at :12001).
# Invoked via: op run --env-file .env.brain.tpl -- bash scripts/dev.sh

set -uo pipefail
set -m  # job control: each background job gets its own pgid (so we can signal whole trees)

BRAIN="$HOME/.local/bin/brain"
_CLEANUP_DONE=0
_DAEMON_WAS_LOADED=0  # set by stop_system_daemon() if we need to restart on cleanup

# ── System daemon control (cross-platform) ────────────────────────────────────
# The brain system daemon (launchd on macOS, systemd --user on Linux) holds
# port 12000 and is configured to auto-restart. During dev we MUST stop it
# completely — the in-process "park via signals" handoff has been unreliable
# (KeepAlive respawns it within seconds, racing the dev binary for the port).
# We unload it on start, reload it on cleanup so the system returns to its
# normal state when the dev session ends.

OS_KIND=""
case "$(uname -s)" in
  Darwin)  OS_KIND="macos" ;;
  Linux)   OS_KIND="linux" ;;
  *)       OS_KIND="other" ;;
esac

stop_system_daemon() {
  case "$OS_KIND" in
    macos)
      local plist="$HOME/Library/LaunchAgents/com.brain.daemon.plist"
      if [[ -f "$plist" ]] && launchctl list 2>/dev/null | grep -q "com.brain.daemon"; then
        echo "  stopping launchd daemon (com.brain.daemon)..."
        launchctl unload "$plist" 2>/dev/null || true
        _DAEMON_WAS_LOADED=1
      fi
      ;;
    linux)
      # `is-enabled` returns 0 only for enabled units; covers the install case.
      if systemctl --user is-enabled brain.service >/dev/null 2>&1; then
        echo "  stopping systemd --user daemon (brain.service)..."
        systemctl --user stop brain.service 2>/dev/null || true
        _DAEMON_WAS_LOADED=1
      fi
      ;;
  esac
}

start_system_daemon() {
  [[ $_DAEMON_WAS_LOADED -eq 1 ]] || return 0
  case "$OS_KIND" in
    macos)
      local plist="$HOME/Library/LaunchAgents/com.brain.daemon.plist"
      [[ -f "$plist" ]] && launchctl load "$plist" 2>/dev/null || true
      ;;
    linux)
      systemctl --user start brain.service 2>/dev/null || true
      ;;
  esac
}

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
  start_system_daemon
}
trap 'cleanup; exit 0' INT TERM

# ── Refresh external rebuy specs ──────────────────────────────────────────────
echo "  syncing rebuy specs..."
"$BRAIN" spec sync --all 2>&1 | sed 's/^/[specs]    /' || true

# ── Take port 12000 ───────────────────────────────────────────────────────────
stop_system_daemon
# Belt-and-braces: clear the stale state file the (now-stopped) daemon left
# behind, plus anything still listening on dev ports.
rm -f "$HOME/.brain/state.json"
for port in 12000 12001; do
  while IFS= read -r pid; do
    echo "  clearing :$port (pid $pid)"
    kill "$pid" 2>/dev/null || true
  done < <(lsof -ti tcp:"$port" 2>/dev/null)
done
sleep 0.3

# This script's PID stays alive across cargo-watch rebuilds — used by 'brain serve --dev'
# to register the dev session in state.
export BRAIN_DEV_PARENT_PID=$$

echo ""
echo "  brain  →  http://localhost:12000  (rust + vite HMR)"
echo ""

# ── Start dev servers ─────────────────────────────────────────────────────────
BRAIN_LOG=trace cargo watch -q -c -C projects/server \
  -w src -w Cargo.toml \
  -x build \
  -s 'BRAIN_LOG=trace ./target/debug/brain serve --dev' 2>&1 | \
  while IFS= read -r line; do
    echo "[server]   $line"
    echo "$line" | grep -q "listening on" && \
      (sleep 0.5 && cd projects/frontend && npm run gen 2>&1 | sed 's/^/[gen]      /') &
  done &

(cd projects/frontend && npm run dev 2>&1 | sed 's/^/[frontend] /') &

wait
