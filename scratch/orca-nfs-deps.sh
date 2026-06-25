#!/bin/sh
# orca-nfs-deps.sh — make NFS-dependent docker containers self-healing.
#
# The pool mounts under /mnt/pool are autofs (automount-on-access), so a
# plain `mount -a` at boot is a no-op and docker can start a container before
# its NFS bind source is live — the container then dies with exit 128, or
# (worse) binds an empty local dir and silently writes to the root disk.
#
# This script:
#   1. Probes each required mount with REAL I/O, which triggers autofs, and
#      waits (up to MAX_WAIT) until every mount actually answers.
#   2. (Re)starts any docker container that binds a /mnt/pool path but is not
#      currently running — discovered dynamically, no hardcoded list.
#
# Idempotent and safe to run both at boot (via /etc/local.d) and on a timer
# (via cron) so it also recovers containers killed by a mid-life NFS blip.
#
# Env: MAX_WAIT (default 90s), REQUIRED (default the two pool mounts).
set -u

REQUIRED="${REQUIRED:-/mnt/pool/data /mnt/pool/downloads}"
MAX_WAIT="${MAX_WAIT:-90}"

log() { logger -t orca-nfs-deps -- "$*" 2>/dev/null; echo "[orca-nfs-deps] $*"; }

probe() { timeout 5 stat -- "$1" >/dev/null 2>&1; }

# 1. Wait for every required mount to answer real I/O (triggers autofs).
waited=0
for m in $REQUIRED; do
  while ! probe "$m"; do
    if [ "$waited" -ge "$MAX_WAIT" ]; then
      log "TIMEOUT waiting for $m after ${MAX_WAIT}s; aborting (will retry next run)"
      exit 1
    fi
    sleep 2
    waited=$((waited + 2))
  done
done
log "all NFS mounts live (waited ${waited}s)"

# 2. (Re)start docker containers that depend on a pool mount but aren't running.
command -v docker >/dev/null 2>&1 || exit 0
if command -v rc-service >/dev/null 2>&1; then
  rc-service docker status 2>/dev/null | grep -q started || {
    log "docker not started yet; nothing to do"
    exit 0
  }
fi

restarted=0
for c in $(docker ps -a --format '{{.Names}}' 2>/dev/null); do
  binds=$(docker inspect -f '{{range .Mounts}}{{.Source}} {{end}}' "$c" 2>/dev/null || true)
  case " $binds " in
    *" /mnt/pool/"*)
      state=$(docker inspect -f '{{.State.Status}}' "$c" 2>/dev/null || echo unknown)
      if [ "$state" != "running" ]; then
        log "restarting NFS-dependent container $c (was: $state)"
        if docker start "$c" >/dev/null 2>&1; then
          restarted=$((restarted + 1))
        else
          log "FAILED to start $c"
        fi
      fi
      ;;
  esac
done
log "done (restarted ${restarted} container(s))"
