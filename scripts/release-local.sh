#!/usr/bin/env bash
# Local equivalent of .github/workflows/release.yml — used when GitHub Actions
# minutes are exhausted. Builds host target only (aarch64-apple-darwin) and
# pushes artifacts to GitHub releases via `gh`.
#
# Usage:
#   scripts/release-local.sh rc <patch|minor|major>   — cut + publish RC
#   scripts/release-local.sh promote                  — promote latest RC to stable
#
# Mirrors the workflow's version math, tag scheme, changelog format, and
# RC-then-stable two-step. Only difference: no gates, since you're the human.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TARGET="aarch64-apple-darwin"
ASSET="orca-${TARGET}"
SERVER_TOML="projects/server/Cargo.toml"

die() { echo "error: $*" >&2; exit 1; }
log() { echo "→ $*"; }

require_clean_tree() {
  if ! git diff --quiet || ! git diff --cached --quiet; then
    die "working tree has uncommitted changes — commit or stash first"
  fi
}

sync_with_origin() {
  # Fetch + auto-reconcile main with origin/main so the user doesn't have to
  # manage divergence manually. Behind → pull --rebase. Ahead → leave (push
  # will send the commits). Diverged → rebase; fail loudly only on conflict.
  local branch local remote base
  branch=$(git rev-parse --abbrev-ref HEAD)
  [ "$branch" = "main" ] || die "must be on 'main' to release (current: $branch)"
  git fetch --quiet origin main
  local=$(git rev-parse HEAD)
  remote=$(git rev-parse origin/main)
  base=$(git merge-base HEAD origin/main)

  if [ "$local" = "$remote" ]; then
    return 0
  elif [ "$local" = "$base" ]; then
    log "local behind origin — rebasing"
    git pull --rebase --autostash origin main
  elif [ "$remote" = "$base" ]; then
    log "local has $(git rev-list --count origin/main..HEAD) unpushed commit(s) — will push with release"
  else
    log "local diverged from origin — attempting rebase"
    git pull --rebase --autostash origin main || die "rebase failed — resolve conflicts and re-run"
  fi
}

drop_stale_local_tag() {
  # If we computed a tag that already exists locally but not on the remote,
  # it's a leftover from an earlier failed run. Delete + recompute caller-side.
  local tag="$1"
  if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null 2>&1; then
    if ! git ls-remote --tags --exit-code origin "refs/tags/${tag}" >/dev/null 2>&1; then
      log "dropping stale local tag ${tag} (not on remote — leftover from prior run)"
      git tag -d "$tag" >/dev/null
    fi
  fi
}

require_tools() {
  command -v gh    >/dev/null || die "gh CLI not installed"
  command -v cargo >/dev/null || die "cargo not installed"
  gh auth status >/dev/null 2>&1 || die "gh not authenticated (run: gh auth login)"
}

# Rollback state — set as cmd_rc/cmd_promote progress, consumed by trap on ERR.
RB_TAG=""
RB_COMMIT=0
RB_CARGO=0
RB_PUSHED=0

rollback() {
  local code=$?
  trap - ERR EXIT
  set +e
  if [ "$RB_PUSHED" -eq 1 ]; then
    log "rollback: tag + commit already pushed to origin — leaving state intact"
    log "         (delete the remote tag + GitHub release manually if needed)"
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
    log "  reverting ${SERVER_TOML} + Cargo.lock"
    git checkout -- "$SERVER_TOML" Cargo.lock 2>/dev/null || true
  fi
  exit "$code"
}

current_cargo_version() {
  grep '^version' "$SERVER_TOML" | head -1 | sed 's/version = "\(.*\)"/\1/'
}

write_cargo_version() {
  local new="$1" current
  current=$(current_cargo_version)
  # macOS sed needs '' after -i
  sed -i '' "0,/^version = \"${current}\"/s//version = \"${new}\"/" "$SERVER_TOML"
  cargo update -p orca --precise "$new" 2>/dev/null || cargo update -p orca || true
}

compute_rc() {
  # In: $1 = patch|minor|major
  # Out: prints "STABLE_VERSION RC_VERSION PREVIOUS_STABLE"
  local bump="$1"
  git fetch --tags --quiet

  local latest_stable
  latest_stable=$(git tag -l 'v[0-9]*' \
    | { grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true; } \
    | sort -V | tail -1)
  latest_stable=${latest_stable:-v0.0.0}

  local major minor patch
  IFS='.' read -r major minor patch <<< "${latest_stable#v}"
  case "$bump" in
    major) major=$((major+1)); minor=0; patch=0 ;;
    minor) minor=$((minor+1)); patch=0 ;;
    patch) patch=$((patch+1)) ;;
    *) die "bump must be patch|minor|major" ;;
  esac
  local next_stable="${major}.${minor}.${patch}"

  local latest_rc n
  latest_rc=$(git tag -l "v${next_stable}-rc.*" | sort -V | tail -1)
  if [ -z "$latest_rc" ]; then
    n=1
  else
    n=${latest_rc##*rc.}; n=$((n+1))
  fi
  echo "$next_stable" "${next_stable}-rc.${n}" "$latest_stable"
}

run_checks() {
  log "cargo fmt --check"
  cargo fmt --all -- --check
  log "cargo clippy"
  RUSTFLAGS="-D warnings" cargo clippy --all-targets -- -D warnings
  log "SDK isolation"
  if cargo tree -p orca-sdk 2>&1 | grep -E "orca-server|orca-commands|orca-conversation|orca-agents|orca-llm|orca-scanner|rust-embed"; then
    die "server-only crate found in orca-sdk dependency tree"
  fi
  log "cargo test (workspace)"
  if command -v cargo-nextest >/dev/null 2>&1; then
    cargo nextest run --workspace --release --no-fail-fast
  else
    cargo test --workspace --release --no-fail-fast
  fi
  log "doctests"
  cargo test --doc --workspace --release --no-fail-fast
}

build_orca_binary() {
  log "building frontend + embedding into release binary (host=${TARGET})"
  # Mirror Makefile `build` flow but pinned to TARGET.
  cargo build --manifest-path "$SERVER_TOML"
  target/debug/orca spec dump > /tmp/orca-openapi.json
  target/debug/orca spec sync --all || true
  ( cd projects/frontend && npm ci && npx tsx scripts/gen.ts --file /tmp/orca-openapi.json && npm run build )
  cargo build --release --target "$TARGET" --manifest-path "$SERVER_TOML"

  mkdir -p dist-release
  cp "target/${TARGET}/release/orca" "dist-release/${ASSET}"
  ( cd dist-release && shasum -a 256 "$ASSET" > "${ASSET}.sha256" )
  cat dist-release/"${ASSET}.sha256"
}

generate_changelog() {
  # $1 = previous stable tag, $2 = new tag, $3 = "rc"|"stable", $4 = optional notes
  # SIGPIPE from `head` closing early kills `git log | grep | head` pipes under
  # pipefail. Disable for this function — changelog is best-effort anyway.
  set +o pipefail
  local prev="$1" new="$2" kind="$3" notes="${4:-}"
  local range commits
  if [ "$prev" = "v0.0.0" ]; then range="HEAD"; else range="${prev}..HEAD"; fi
  commits=$(git log "$range" --pretty=format:"%s" | grep -v '^chore: release v' | head -100)

  local repo
  repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)

  section() {
    local title="$1"; shift
    local items=""
    for prefix in "$@"; do
      items="$items$(echo "$commits" | grep -i "^${prefix}[:(]" | sed 's/^/- /' || true)"$'\n'
    done
    [ -n "$(echo "$items" | tr -d '[:space:]')" ] && printf "### %s\n%s\n" "$title" "$items"
    return 0  # empty section is fine — don't trip errexit in caller
  }

  {
    [ -n "$notes" ] && printf "%s\n\n---\n\n" "$notes"
    if [ "$kind" = "rc" ]; then
      printf "> **Pre-release** \`rc\` — pending stable promotion.\n\n"
    else
      printf "> Promoted from RC. Binaries are identical.\n\n"
    fi
    printf "## What's Changed\n\n"
    section 'Features'    feat feature
    section 'Bug Fixes'   fix bug
    section 'Performance' perf
    section 'Refactoring' refactor refact
    section 'Build / CI'  build ci chore
    section 'Docs'        docs

    printf "\n## Installation\n\n\`\`\`sh\n# Apple Silicon\ncurl -Lo orca https://github.com/%s/releases/download/%s/%s\n\nchmod +x orca && mv orca ~/.local/bin/orca\nxattr -d com.apple.quarantine ~/.local/bin/orca\n\`\`\`\n\n" "$repo" "$new" "$ASSET"
    printf "**Full diff:** [%s → %s](https://github.com/%s/compare/%s...%s)\n" "$prev" "$new" "$repo" "$prev" "$new"
    printf "\n_Built locally on %s — host target only (%s)._\n" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$TARGET"
  } > /tmp/orca-changelog.md
  set -o pipefail
}

cmd_rc() {
  local bump="${1:-}"; [ -n "$bump" ] || die "usage: release-local.sh rc <patch|minor|major>"
  require_tools
  require_clean_tree
  sync_with_origin

  read -r STABLE RC PREV < <(compute_rc "$bump")
  # Recompute after dropping any stale local tag for this RC, in case a prior
  # run tagged but never pushed.
  drop_stale_local_tag "v${RC}"
  read -r STABLE RC PREV < <(compute_rc "$bump")
  log "previous stable : $PREV"
  log "next stable     : v$STABLE"
  log "next rc         : v$RC"

  run_checks
  build_orca_binary

  # From here on, any failure must roll back. Anything before this point only
  # reads state (or drops a stale local-only tag, which is already idempotent).
  trap rollback ERR

  log "bumping ${SERVER_TOML} → ${RC}"
  write_cargo_version "$RC"
  RB_CARGO=1

  log "commit + tag + push"
  git add "$SERVER_TOML"
  git check-ignore -q Cargo.lock || git add Cargo.lock
  if ! git diff --cached --quiet; then
    git commit -m "chore: release v${RC}"
    RB_COMMIT=1
  fi
  git tag -a "v${RC}" -m "orca v${RC}"
  RB_TAG="v${RC}"
  git push origin HEAD --tags
  RB_PUSHED=1

  generate_changelog "$PREV" "v${RC}" "rc"

  log "creating GitHub pre-release v${RC}"
  gh release create "v${RC}" \
    --title "orca v${RC}" \
    --notes-file /tmp/orca-changelog.md \
    --prerelease \
    "dist-release/${ASSET}" \
    "dist-release/${ASSET}.sha256"

  log "done — review the release, then run: scripts/release-local.sh promote"
}

cmd_promote() {
  require_tools
  # Don't require a fully clean tree — the rc build regenerates frontend
  # client/types files that are never committed. Just check Cargo.toml is
  # clean so the version bump applies cleanly.
  if ! git diff --quiet -- "$SERVER_TOML" || ! git diff --cached --quiet -- "$SERVER_TOML"; then
    die "$SERVER_TOML has uncommitted changes — commit or revert first"
  fi
  sync_with_origin

  git fetch --tags --quiet
  local latest_rc rc_version stable_version stable_tag prev
  latest_rc=$(git tag -l 'v*-rc.*' | sort -V | tail -1)
  [ -n "$latest_rc" ] || die "no RC tag found"
  rc_version=${latest_rc#v}
  stable_version=${rc_version%-rc.*}
  stable_tag="v${stable_version}"

  if git rev-parse -q --verify "refs/tags/${stable_tag}" >/dev/null; then
    die "stable tag ${stable_tag} already exists"
  fi

  prev=$(git tag -l 'v[0-9]*' \
    | { grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true; } \
    | sort -V | tail -1)
  prev=${prev:-v0.0.0}

  log "promoting ${latest_rc} → ${stable_tag}"

  log "downloading RC artifacts"
  rm -rf dist-release && mkdir -p dist-release
  gh release download "$latest_rc" --dir dist-release --pattern "orca-*" --pattern "*.sha256"
  ls -lh dist-release/

  trap rollback ERR

  log "bumping ${SERVER_TOML} → ${stable_version}"
  write_cargo_version "$stable_version"
  RB_CARGO=1

  log "commit + tag + push"
  git add "$SERVER_TOML"
  git check-ignore -q Cargo.lock || git add Cargo.lock
  if ! git diff --cached --quiet; then
    git commit -m "chore: release ${stable_tag}"
    RB_COMMIT=1
  fi
  git tag -a "$stable_tag" -m "orca ${stable_tag} (promoted from ${latest_rc})"
  RB_TAG="$stable_tag"
  git push origin HEAD --tags
  RB_PUSHED=1

  generate_changelog "$prev" "$stable_tag" "stable"

  log "creating GitHub release ${stable_tag}"
  # List assets explicitly — globs match overlapping sets (orca-* also catches
  # *.sha256), which produces duplicate uploads and a 404 from gh.
  gh release create "$stable_tag" \
    --title "orca ${stable_tag}" \
    --notes-file /tmp/orca-changelog.md \
    "dist-release/${ASSET}" \
    "dist-release/${ASSET}.sha256"

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
