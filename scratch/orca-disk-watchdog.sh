#!/bin/sh
# orca-disk-watchdog.sh — keep a Docker host's root filesystem from filling up.
#
# Runs on a timer. When root usage crosses WARN_PCT it performs tiered,
# data-safe cleanups (most-conservative first) and re-measures after each
# tier, stopping as soon as usage drops back under TARGET_PCT. If usage is
# still above CRIT_PCT after every tier, it fires an ntfy alert so a human
# looks before anything risky happens.
#
# Deliberately NEVER touches:
#   - NFS mounts (only acts on the root device)
#   - /mnt/alpha, /mnt/pool, or any download/media/appdata data
# It only reclaims regenerable junk: docker logs/images/build cache,
# the systemd journal, package caches, and orca's local build artifacts
# (~/.orca/dev, ~/.rustup, ~/.cargo caches) — which should not exist on a
# host that only ever pulls released orca binaries.
#
# Usage: orca-disk-watchdog.sh [--dry-run]
# Env overrides: WARN_PCT (default 80) TARGET_PCT (75) CRIT_PCT (90)
#                ORCA_HOME (/var/lib/orca) NTFY_URL (optional)

set -eu

WARN_PCT="${WARN_PCT:-80}"
TARGET_PCT="${TARGET_PCT:-75}"
CRIT_PCT="${CRIT_PCT:-90}"
ORCA_HOME="${ORCA_HOME:-/var/lib/orca}"
NTFY_URL="${NTFY_URL:-}"
DRY_RUN=false
[ "${1:-}" = "--dry-run" ] && DRY_RUN=true

log() { echo "[orca-disk-watchdog] $*"; }

root_pct() { df -P / | awk 'NR==2 {gsub(/%/,"",$5); print $5}'; }

run() {
  if $DRY_RUN; then log "DRY: $*"; else sh -c "$*"; fi
}

# True while root usage is still at/above TARGET_PCT (keep cleaning).
over_target() { [ "$(root_pct)" -ge "$TARGET_PCT" ]; }

PCT=$(root_pct)
log "root usage ${PCT}% (warn=${WARN_PCT} target=${TARGET_PCT} crit=${CRIT_PCT})"
if [ "$PCT" -lt "$WARN_PCT" ]; then
  log "below warn threshold; nothing to do"
  exit 0
fi

# --- Tier 1: docker container logs > 50M (truncate, never delete) ---
log "tier 1: truncating oversized docker json logs"
for f in $(find /var/lib/docker/containers -name '*-json.log' -size +50M 2>/dev/null); do
  run ": > '$f'"
done
over_target || { log "recovered after tier 1 (now $(root_pct)%)"; exit 0; }

# --- Tier 2: docker dangling images + build cache ---
log "tier 2: docker image/builder prune"
run "docker image prune -f >/dev/null 2>&1 || true"
run "docker builder prune -f >/dev/null 2>&1 || true"
over_target || { log "recovered after tier 2 (now $(root_pct)%)"; exit 0; }

# --- Tier 3: journal + package caches ---
log "tier 3: journal vacuum + package cache clean"
run "journalctl --vacuum-size=200M >/dev/null 2>&1 || true"
run "command -v apt-get >/dev/null 2>&1 && apt-get clean || true"
run "command -v apk >/dev/null 2>&1 && rm -rf /var/cache/apk/* || true"
over_target || { log "recovered after tier 3 (now $(root_pct)%)"; exit 0; }

# --- Tier 4: orca local build junk (should not exist; host pulls releases) ---
log "tier 4: removing orca build artifacts under ${ORCA_HOME}"
for d in "$ORCA_HOME/.orca/dev" "$ORCA_HOME/.rustup" "$ORCA_HOME/.cargo/registry" "$ORCA_HOME/.cargo/git"; do
  [ -e "$d" ] || continue
  log "  removing $d"
  run "find '$d' -mindepth 1 -delete 2>/dev/null || true"
  run "rmdir '$d' 2>/dev/null || true"
done
over_target || { log "recovered after tier 4 (now $(root_pct)%)"; exit 0; }

# --- Still critical: alert a human, do nothing risky ---
FINAL=$(root_pct)
log "STILL HIGH after all safe tiers: ${FINAL}%"
if [ "$FINAL" -ge "$CRIT_PCT" ] && [ -n "$NTFY_URL" ]; then
  HOST=$(hostname)
  run "curl -fsS -H 'Title: disk watchdog: ${HOST} root ${FINAL}%' \
        -H 'Priority: urgent' -H 'Tags: warning,floppy_disk' \
        -d 'Root fs at ${FINAL}% after all safe cleanups. Manual review needed (downloads/appdata not auto-deleted).' \
        '$NTFY_URL' >/dev/null 2>&1 || true"
fi
exit 1
