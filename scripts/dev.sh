#!/usr/bin/env bash
# Dev mode: Rust + Vite on :12000 (Rust proxies non-API to Vite at :12001).
# Invoked via: op run --env-file .env.orca.tpl -- bash scripts/dev.sh

set -uo pipefail
set -m  # job control: each background job gets its own pgid (so we can signal whole trees)

ORCA="$HOME/.local/bin/orca"
_CLEANUP_DONE=0
_DAEMON_WAS_LOADED=0  # set by stop_system_daemon() if we need to restart on cleanup
_SERVER_PID=
_FRONTEND_PID=

# ── System daemon control (cross-platform) ────────────────────────────────────
OS_KIND=""
case "$(uname -s)" in
  Darwin)  OS_KIND="macos" ;;
  Linux)   OS_KIND="linux" ;;
  *)       OS_KIND="other" ;;
esac

stop_system_daemon() {
  case "$OS_KIND" in
    macos)
      local plist="$HOME/Library/LaunchAgents/com.orca.daemon.plist"
      if [[ -f "$plist" ]] && launchctl list 2>/dev/null | grep -q "com.orca.daemon"; then
        echo "  stopping launchd daemon (com.orca.daemon)..."
        launchctl unload "$plist" 2>/dev/null || true
        _DAEMON_WAS_LOADED=1
      fi
      ;;
    linux)
      if systemctl --user is-enabled orca.service >/dev/null 2>&1; then
        echo "  stopping systemd --user daemon (orca.service)..."
        systemctl --user stop orca.service 2>/dev/null || true
        _DAEMON_WAS_LOADED=1
      fi
      ;;
  esac
}

start_system_daemon() {
  [[ $_DAEMON_WAS_LOADED -eq 1 ]] || return 0
  case "$OS_KIND" in
    macos)
      local plist="$HOME/Library/LaunchAgents/com.orca.daemon.plist"
      [[ -f "$plist" ]] && launchctl load "$plist" 2>/dev/null || true
      ;;
    linux)
      systemctl --user start orca.service 2>/dev/null || true
      ;;
  esac
}

cleanup() {
  [[ $_CLEANUP_DONE -eq 1 ]] && return
  _CLEANUP_DONE=1
  echo ""
  echo "  stopping dev session..."
  trap '' TERM
  [[ -n "${_SERVER_PID:-}" ]]   && kill -- -"$_SERVER_PID"   2>/dev/null || true
  [[ -n "${_FRONTEND_PID:-}" ]] && kill -- -"$_FRONTEND_PID" 2>/dev/null || true
  sleep 0.3
  [[ -n "${_SERVER_PID:-}" ]]   && kill -KILL -- -"$_SERVER_PID"   2>/dev/null || true
  [[ -n "${_FRONTEND_PID:-}" ]] && kill -KILL -- -"$_FRONTEND_PID" 2>/dev/null || true
  start_system_daemon
}
trap 'cleanup; exit 0' INT TERM

# ── Refresh external rebuy specs ──────────────────────────────────────────────
echo "  syncing rebuy specs..."
"$ORCA" spec sync --all 2>&1 | sed 's/^/[specs]    /' || true

# ── Take port 12000 ───────────────────────────────────────────────────────────
stop_system_daemon
rm -f "$HOME/.orca/state.json"
for port in 12000 12001; do
  # -sTCP:LISTEN restricts to listening sockets — without it lsof returns every
  # process with *any* connection on that port (including your browser holding
  # open HMR WebSockets), which we'd then SIGTERM.
  while IFS= read -r pid; do
    echo "  clearing :$port (pid $pid)"
    kill "$pid" 2>/dev/null || true
  done < <(lsof -ti tcp:"$port" -sTCP:LISTEN 2>/dev/null)
done
sleep 0.3

export ORCA_DEV_PARENT_PID=$$

echo ""
echo "  orca  →  http://localhost:12000  (rust + vite HMR)"
echo ""

# ── Start dev servers ─────────────────────────────────────────────────────────
ORCA_LOG=trace cargo watch -q -c -C projects/server \
  -w src -w Cargo.toml \
  -x build \
  -s 'ORCA_LOG=trace ../../target/debug/orca serve --dev' 2>&1 | \
  sed 's/^/[server]   /' &
_SERVER_PID=$!

(cd projects/frontend && npm run dev 2>&1 | sed 's/^/[frontend] /') &
_FRONTEND_PID=$!

wait
