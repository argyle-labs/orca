#!/usr/bin/env bash
# Dev mode: Rust + Vite on :12000 (Rust proxies non-API to Vite at :12001).
# Invoked via: op run --env-file .env.orca.tpl -- bash scripts/dev.sh
#
# Flags:
#   --serve-binary   Also cross-compile for linux x86_64 and serve the binary
#                    on :12009 for fleet hot-reload. Peers running
#                    `orca update --source http://<ip>:12009` auto-update
#                    within 10s of each build.

set -uo pipefail
set -m  # job control: each background job gets its own pgid (so we can signal whole trees)

SERVE_BINARY=0
for arg in "$@"; do
  case "$arg" in
    --serve-binary) SERVE_BINARY=1 ;;
    *) echo "unknown flag: $arg"; exit 1 ;;
  esac
done

ORCA="$HOME/.local/bin/orca"
_CLEANUP_DONE=0
_DAEMON_WAS_LOADED=0  # set by stop_system_daemon() if we need to restart on cleanup
_SERVER_PID=
_FRONTEND_PID=
_BINARY_WATCH_PID=
_BINARY_SERVE_PID=

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
  trap '' TERM INT
  # With `set -m`, each `&`-backgrounded pipeline becomes its own job with its
  # own pgid. `$!` captures the PID of the pipeline's last command (sed), but
  # the pgid equals the first command (cargo-watch / npm). So address the jobs
  # by job-spec, which kills the whole pgid regardless of which PID we saw.
  local jobs
  jobs=$(jobs -p)
  for jspec in %1 %2 %3 %4 %5 %6; do
    kill -TERM "$jspec" 2>/dev/null || true
  done
  # Also fall back to direct PIDs in case set -m didn't take or jobs are gone.
  for pid in $jobs; do
    kill -TERM "$pid"        2>/dev/null || true
    kill -TERM -- "-$pid"    2>/dev/null || true
  done
  sleep 0.4
  for jspec in %1 %2 %3 %4 %5 %6; do
    kill -KILL "$jspec" 2>/dev/null || true
  done
  for pid in $jobs; do
    kill -KILL "$pid"        2>/dev/null || true
    kill -KILL -- "-$pid"    2>/dev/null || true
  done
  # Reload launchd daemon in the background so a slow launchctl can't hang exit.
  ( start_system_daemon ) &
  disown 2>/dev/null || true
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
# Respawn the server if it dies between rebuilds. cargo-watch only runs the
# command on file changes — without this loop, a server panic leaves the
# backend dead until the next save. Loop exits when the script's trap kills
# the whole process group.
DEV_LINUX_TARGET=x86_64-unknown-linux-gnu
DEV_BINARY=target/${DEV_LINUX_TARGET}/release/orca
DEV_LOG=trace,hyper=warn,mio=warn,h2=warn,reqwest=warn,rustls=warn,tower_http=warn,tungstenite=warn
DEV_SERVER_CMD='while true; do ORCA_LOG='"$DEV_LOG"' ../../target/debug/orca serve --dev; echo "  [server exited — respawning in 1s]"; sleep 1; done'

if [[ $SERVE_BINARY -eq 1 ]]; then
  # Linux release build runs as a background -s step after the debug build so
  # both share one cargo-watch process and avoid fighting over the Cargo lock.
  DEV_LINUX_BUILD_CMD="cargo build --release --target ${DEV_LINUX_TARGET} 2>&1 | sed 's/^/[linux]    /' &"
  ORCA_LOG=$DEV_LOG cargo watch -C projects/server \
    -w src -w Cargo.toml \
    -x build \
    -s "$DEV_LINUX_BUILD_CMD" \
    -s "$DEV_SERVER_CMD" 2>&1 | sed 's/^/[server]   /' &
else
  ORCA_LOG=$DEV_LOG cargo watch -C projects/server \
    -w src -w Cargo.toml \
    -x build \
    -s "$DEV_SERVER_CMD" 2>&1 | sed 's/^/[server]   /' &
fi
_SERVER_PID=$!

(cd projects/frontend && npm run dev 2>&1 | sed 's/^/[frontend] /') &
_FRONTEND_PID=$!

if [[ $SERVE_BINARY -eq 1 ]]; then
  echo "  binary   →  http://0.0.0.0:12009  (linux x86_64 fleet hot-reload)"
  echo "              on each peer: orca update --source http://<mint-ip>:12009"
  echo ""
  # Retry loop: the first run may use a stale debug binary that predates dev-serve.
  # Cargo watch will rebuild and the loop retries until it works.
  (while true; do
     until [[ -f target/debug/orca ]]; do sleep 1; done
     target/debug/orca dev-serve --binary "$DEV_BINARY" --port 12009 2>&1 | sed 's/^/[serve]    /'
     echo "  [serve exited — retrying in 3s]"
     sleep 3
   done) &
  _BINARY_SERVE_PID=$!
fi

wait
