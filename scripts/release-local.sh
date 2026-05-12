#!/usr/bin/env bash
# Local release orchestrator. All logic lives in scripts/release-lib.sh —
# this file is just the CLI surface and rollback state machine.
#
# Usage:
#   scripts/release-local.sh rc <patch|minor|major>   — cut + publish RC
#   scripts/release-local.sh promote                  — promote latest RC to stable
#
# Knobs (env):
#   RELEASE_PARALLEL_TARGETS   max targets built in parallel (default: cores/4)
#   RELEASE_CARGO_JOBS         cargo -j per target build   (default: cores/parallel)
#   RELEASE_TARGETS            override target list (space-separated)
#   RELEASE_FEATURES           cargo features (default: ui; empty = headless)

set -euo pipefail

# shellcheck source=./release-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/release-lib.sh"
cd "$REPO_ROOT"

# Resolve target list once. RELEASE_TARGETS env overrides defaults.
if [ -n "${RELEASE_TARGETS:-}" ]; then
  # shellcheck disable=SC2206
  TARGETS=( $RELEASE_TARGETS )
else
  mapfile_to_array TARGETS default_targets
fi

# ── rollback state ──────────────────────────────────────────────────────────
RB_TAG=""
RB_COMMIT=0
RB_CARGO=0
RB_PUSHED=0

rollback() {
  local code=$?
  trap - ERR EXIT
  set +e
  if [ "$RB_PUSHED" -eq 1 ]; then
    log "rollback: tag + commit already pushed — leaving state intact"
    log "         (delete remote tag + GitHub release manually if needed)"
    exit "$code"
  fi
  log "rollback: undoing partial release state"
  if [ -n "$RB_TAG" ] && git rev-parse -q --verify "refs/tags/${RB_TAG}" >/dev/null 2>&1; then
    log "  deleting local tag ${RB_TAG}"
    git tag -d "$RB_TAG" >/dev/null
  fi
  if [ "$RB_COMMIT" -eq 1 ]; then
    log "  reverting release commit (git reset --hard HEAD~1)"
    git reset --hard HEAD~1 >/dev/null
  elif [ "$RB_CARGO" -eq 1 ]; then
    log "  reverting Cargo.toml + Cargo.lock"
    git checkout -- projects/server/Cargo.toml Cargo.lock 2>/dev/null || true
  fi
  exit "$code"
}

# ── commands ────────────────────────────────────────────────────────────────

# Wraps bump_and_build so RB_CARGO flips immediately. Pure orchestration.
bump_and_build_with_rollback() {
  RB_CARGO=1
  bump_and_build "$1" "${TARGETS[@]}"
}

cmd_rc() {
  local bump="${1:-}"; [ -n "$bump" ] || die "usage: release-local.sh rc <patch|minor|major>"
  require_release_tools "${TARGETS[@]}"
  require_clean_tree
  sync_with_origin

  read -r STABLE RC PREV < <(compute_rc_version "$bump")
  drop_stale_local_tag "v${RC}"
  read -r STABLE RC PREV < <(compute_rc_version "$bump")
  log "previous stable : $PREV"
  log "next stable     : v$STABLE"
  log "next rc         : v$RC"

  run_release_checks

  trap rollback ERR
  bump_and_build_with_rollback "$RC"

  log "commit + tag + push"
  git add projects/server/Cargo.toml
  git check-ignore -q Cargo.lock || git add Cargo.lock
  if ! git diff --cached --quiet; then
    git commit -m "chore: release v${RC}"
    RB_COMMIT=1
  fi
  git tag -a "v${RC}" -m "orca v${RC}"
  RB_TAG="v${RC}"
  git push origin HEAD --tags
  RB_PUSHED=1

  generate_changelog "$PREV" "v${RC}" "rc" "" "${TARGETS[@]}"

  log "creating GitHub pre-release v${RC}"
  # shellcheck disable=SC2046
  gh release create "v${RC}" \
    --title "orca v${RC}" \
    --notes-file /tmp/orca-changelog.md \
    --prerelease \
    "${REPO_ROOT}/scripts/install.sh" \
    $(release_asset_paths "${TARGETS[@]}")

  log "done — review the release, then run: scripts/release-local.sh promote"
}

cmd_promote() {
  require_release_tools "${TARGETS[@]}"
  if ! git diff --quiet -- projects/server/Cargo.toml \
     || ! git diff --cached --quiet -- projects/server/Cargo.toml; then
    die "projects/server/Cargo.toml has uncommitted changes — commit or revert first"
  fi
  sync_with_origin

  git fetch --tags --quiet
  local latest_rc rc_version stable_version stable_tag prev
  latest_rc=$(git tag -l 'v*-rc.*' | sort -V | tail -1)
  [ -n "$latest_rc" ] || die "no RC tag found"
  rc_version=${latest_rc#v}
  stable_version=${rc_version%-rc.*}
  stable_tag="v${stable_version}"

  git rev-parse -q --verify "refs/tags/${stable_tag}" >/dev/null \
    && die "stable tag ${stable_tag} already exists"

  prev=$(git tag -l 'v[0-9]*' \
    | { grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true; } \
    | sort -V | tail -1)
  prev=${prev:-v0.0.0}

  log "promoting ${latest_rc} → ${stable_tag}"

  trap rollback ERR
  bump_and_build_with_rollback "$stable_version"

  log "commit + tag + push"
  git add projects/server/Cargo.toml
  git check-ignore -q Cargo.lock || git add Cargo.lock
  if ! git diff --cached --quiet; then
    git commit -m "chore: release ${stable_tag}"
    RB_COMMIT=1
  fi
  git tag -a "$stable_tag" -m "orca ${stable_tag} (promoted from ${latest_rc})"
  RB_TAG="$stable_tag"
  git push origin HEAD --tags
  RB_PUSHED=1

  generate_changelog "$prev" "$stable_tag" "stable" "" "${TARGETS[@]}"
  prepend_changelog "$stable_tag"

  # Include CHANGELOG.md in the release commit if it changed
  if ! git diff --quiet -- CHANGELOG.md || ! git diff --cached --quiet -- CHANGELOG.md; then
    git add CHANGELOG.md
    git commit --amend --no-edit
  fi

  log "creating GitHub release ${stable_tag}"
  # shellcheck disable=SC2046
  gh release create "$stable_tag" \
    --title "orca ${stable_tag}" \
    --notes-file /tmp/orca-changelog.md \
    "${REPO_ROOT}/scripts/install.sh" \
    $(release_asset_paths "${TARGETS[@]}")

  log "marking ${latest_rc} as superseded"
  local repo; repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
  gh release edit "$latest_rc" \
    --notes "> **Superseded** — promoted to stable [${stable_tag}](https://github.com/${repo}/releases/tag/${stable_tag})." \
    --prerelease

  log "done — ${stable_tag} published"
}

case "${1:-}" in
  rc)      shift; cmd_rc "$@" ;;
  promote) shift; cmd_promote "$@" ;;
  *) die "usage: release-local.sh {rc <patch|minor|major> | promote}" ;;
esac
