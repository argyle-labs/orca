#!/usr/bin/env bash
# Dev-loop host build. Used by `make build` / `make build-headless` / `make
# deploy` so the day-to-day developer path goes through the same compile
# functions as the release pipeline — one bug, one fix.
#
# Usage:
#   scripts/build-host.sh            # default features (ui — embedded frontend)
#   scripts/build-host.sh --headless # no frontend embed
#
# Output: target/<host-triple>/release/orca
# Does NOT stage into dist-release/ — that's a release-only concern.

set -euo pipefail

# shellcheck source=./release-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/release-lib.sh"
cd "$REPO_ROOT"

while [ $# -gt 0 ]; do
  case "$1" in
    --headless) RELEASE_FEATURES=""; shift ;;
    *) die "unknown flag: $1" ;;
  esac
done

target=$(host_target)
jobs=$(release_cargo_jobs 1)

# Frontend only needs building when the `ui` feature is enabled.
[ -n "$RELEASE_FEATURES" ] && build_frontend

cargo_build_target "$target" "$jobs"

bin="${REPO_ROOT}/target/${target}/release/orca"
log "built → ${bin}"
