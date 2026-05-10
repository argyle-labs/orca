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

require_tools() {
  command -v gh    >/dev/null || die "gh CLI not installed"
  command -v cargo >/dev/null || die "cargo not installed"
  gh auth status >/dev/null 2>&1 || die "gh not authenticated (run: gh auth login)"
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
}

cmd_rc() {
  local bump="${1:-}"; [ -n "$bump" ] || die "usage: release-local.sh rc <patch|minor|major>"
  require_tools
  require_clean_tree

  read -r STABLE RC PREV < <(compute_rc "$bump")
  log "previous stable : $PREV"
  log "next stable     : v$STABLE"
  log "next rc         : v$RC"

  run_checks
  build_orca_binary

  log "bumping ${SERVER_TOML} → ${RC}"
  write_cargo_version "$RC"

  log "commit + tag + push"
  git add "$SERVER_TOML" Cargo.lock
  git diff --cached --quiet || git commit -m "chore: release v${RC}"
  git tag -a "v${RC}" -m "orca v${RC}"
  git push origin HEAD --tags

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
  require_clean_tree

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

  log "bumping ${SERVER_TOML} → ${stable_version}"
  write_cargo_version "$stable_version"

  log "commit + tag + push"
  git add "$SERVER_TOML" Cargo.lock
  git diff --cached --quiet || git commit -m "chore: release ${stable_tag}"
  git tag -a "$stable_tag" -m "orca ${stable_tag} (promoted from ${latest_rc})"
  git push origin HEAD --tags

  generate_changelog "$prev" "$stable_tag" "stable"

  log "creating GitHub release ${stable_tag}"
  gh release create "$stable_tag" \
    --title "orca ${stable_tag}" \
    --notes-file /tmp/orca-changelog.md \
    dist-release/orca-* dist-release/*.sha256

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
