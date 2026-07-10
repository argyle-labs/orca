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

HEADLESS=0
while [ $# -gt 0 ]; do
  case "$1" in
    --headless) HEADLESS=1; shift ;;
    *) die "unknown flag: $1" ;;
  esac
done

# Headless: strip default features (which include `ui`) and skip the frontend
# npm build. The `ui` feature being on-by-default in Cargo.toml means a plain
# build always embeds the frontend; headless must opt out explicitly.
if [ "$HEADLESS" = "1" ]; then
  export RELEASE_NO_DEFAULT_FEATURES=1
  RELEASE_FEATURES=""
fi

target=$(host_target)
jobs=$(release_cargo_jobs 1)

# No frontend build: the web UI is no longer embedded in the orca binary — it
# ships as the out-of-process peacock plugin. (HEADLESS still controls the `ui`
# cargo feature, which now only gates the `/` route, not an embedded dist.)

cargo_build_target "$target" "$jobs"

bin="${REPO_ROOT}/target/${target}/release/orca"
log "built → ${bin}"
