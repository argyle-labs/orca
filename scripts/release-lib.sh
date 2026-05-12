#!/usr/bin/env bash
# Shared release library — single source of truth for the release pipeline.
#
# Used by:
#   - scripts/release-local.sh                 (local orchestrator)
#   - .github/actions/build-orca-target/       (per-target CI build)
#   - .github/actions/compute-version/         (version math)
#   - .github/actions/generate-changelog/      (changelog body)
#   - Future Gitea pipelines (call functions directly via `source`)
#
# Rule: every release-related bug gets fixed here. Surfaces above are thin.
#
# Conventions:
#   - Pure functions; no top-level side effects except setting REPO_ROOT and
#     constants. Sourcing must be idempotent.
#   - Bash 3.2 compatible (macOS) — no associative arrays, no `wait -n`,
#     no `${var,,}`, no `mapfile`.
#   - All cargo invocations honor RELEASE_CARGO_JOBS (defaults: see
#     release_cargo_jobs). Parallel target builds honor RELEASE_PARALLEL_TARGETS.

# Guard against double-sourcing.
[ -n "${ORCA_RELEASE_LIB_SOURCED:-}" ] && return 0
ORCA_RELEASE_LIB_SOURCED=1

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVER_TOML="${REPO_ROOT}/projects/server/Cargo.toml"
DIST_DIR="${REPO_ROOT}/dist-release"

# Default cargo features for release builds. Local + CI both build with the
# embedded frontend so the binary is self-contained. Override with
# RELEASE_FEATURES="" for a headless build.
: "${RELEASE_FEATURES:=ui}"

# Cargo profile. Defaults to `release` (fat LTO, codegen-units=1 — slow build,
# fast binary; required for shipped releases). `make build` overrides to
# `release-fast` (thin LTO, 16 codegen units — uses every core, slightly
# larger/slower binary, fine for dev).
: "${RELEASE_PROFILE:=release}"

# ── target sets ─────────────────────────────────────────────────────────────

# All Linux targets are cross-compiled via cargo-zigbuild from any host.
LINUX_TARGETS=(
  x86_64-unknown-linux-gnu
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-gnu
  aarch64-unknown-linux-musl
)
# macOS targets require a macOS host (no osxcross).
MAC_TARGETS_ALL=(aarch64-apple-darwin x86_64-apple-darwin)

# Default target list for the current host. Callers can override by setting
# RELEASE_TARGETS="t1 t2 ..." in the environment, or by passing the list to
# build_orca_targets directly.
default_targets() {
  local out=()
  case "$(uname -s)" in
    Darwin) out=("${MAC_TARGETS_ALL[@]}" "${LINUX_TARGETS[@]}") ;;
    *)      out=("${LINUX_TARGETS[@]}") ;;
  esac
  printf '%s\n' "${out[@]}"
}

host_target() {
  rustc -vV | awk '/^host:/ {print $2}'
}

# ── log helpers ─────────────────────────────────────────────────────────────

die() { echo "error: $*" >&2; exit 1; }
log() { echo "→ $*"; }

# ── parallelism knobs ───────────────────────────────────────────────────────

# Total logical cores on the host. Portable across macOS/Linux.
detect_cores() {
  if command -v nproc >/dev/null 2>&1; then
    nproc
  elif [ "$(uname -s)" = "Darwin" ]; then
    sysctl -n hw.ncpu
  else
    echo 4
  fi
}

# How many target builds to run concurrently.
#   - Env override: RELEASE_PARALLEL_TARGETS
#   - Default: min(num_targets, cores / 4) — leaves cores for cargo's internal
#     parallelism within each target. Floor of 1.
release_parallel_targets() {
  local num_targets="$1" cores parallel
  if [ -n "${RELEASE_PARALLEL_TARGETS:-}" ]; then
    parallel="$RELEASE_PARALLEL_TARGETS"
  else
    cores=$(detect_cores)
    parallel=$(( cores / 4 ))
    [ "$parallel" -lt 1 ] && parallel=1
  fi
  [ "$parallel" -gt "$num_targets" ] && parallel="$num_targets"
  echo "$parallel"
}

# `cargo build -j N` value per target build.
#   - Env override: RELEASE_CARGO_JOBS
#   - Default: cores / parallel_targets. Floor of 1.
# CI matrix runs ONE target per runner, so callers pass parallel=1 → full cores.
release_cargo_jobs() {
  local parallel="$1" cores jobs
  if [ -n "${RELEASE_CARGO_JOBS:-}" ]; then
    echo "$RELEASE_CARGO_JOBS"
    return
  fi
  cores=$(detect_cores)
  jobs=$(( cores / parallel ))
  [ "$jobs" -lt 1 ] && jobs=1
  echo "$jobs"
}

# ── git + repo state ────────────────────────────────────────────────────────

require_clean_tree() {
  cd "$REPO_ROOT"
  if ! git diff --quiet || ! git diff --cached --quiet; then
    die "working tree has uncommitted changes — commit or stash first"
  fi
}

sync_with_origin() {
  cd "$REPO_ROOT"
  local branch local_sha remote_sha base
  branch=$(git rev-parse --abbrev-ref HEAD)
  [ "$branch" = "main" ] || die "must be on 'main' to release (current: $branch)"
  git fetch --quiet origin main
  local_sha=$(git rev-parse HEAD)
  remote_sha=$(git rev-parse origin/main)
  base=$(git merge-base HEAD origin/main)
  if [ "$local_sha" = "$remote_sha" ]; then
    return 0
  elif [ "$local_sha" = "$base" ]; then
    log "local behind origin — rebasing"
    git pull --rebase --autostash origin main
  elif [ "$remote_sha" = "$base" ]; then
    log "local has $(git rev-list --count origin/main..HEAD) unpushed commit(s) — will push with release"
  else
    log "local diverged from origin — attempting rebase"
    git pull --rebase --autostash origin main || die "rebase failed — resolve conflicts and re-run"
  fi
}

drop_stale_local_tag() {
  local tag="$1"
  cd "$REPO_ROOT"
  if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null 2>&1; then
    if ! git ls-remote --tags --exit-code origin "refs/tags/${tag}" >/dev/null 2>&1; then
      log "dropping stale local tag ${tag} (not on remote — leftover from prior run)"
      git tag -d "$tag" >/dev/null
    fi
  fi
}

# ── tool checks ─────────────────────────────────────────────────────────────

require_release_tools() {
  command -v gh             >/dev/null || die "gh CLI not installed"
  command -v cargo          >/dev/null || die "cargo not installed"
  command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild not installed (cargo install cargo-zigbuild + brew install zig)"
  command -v zig            >/dev/null || die "zig not installed (brew install zig)"
  gh auth status >/dev/null 2>&1 || die "gh not authenticated (run: gh auth login)"
  local t
  for t in "$@"; do
    if ! rustup target list --installed | grep -qx "$t"; then
      log "rust target $t not installed — running: rustup target add $t"
      rustup target add "$t" || die "failed to install rust target: $t"
    fi
  done
}

# ── version manipulation ────────────────────────────────────────────────────

current_cargo_version() {
  grep '^version' "$SERVER_TOML" | head -1 | sed 's/version = "\(.*\)"/\1/'
}

write_cargo_version() {
  local new="$1" current
  current=$(current_cargo_version)
  if [ "$(uname -s)" = "Darwin" ]; then
    sed -i '' "0,/^version = \"${current}\"/s//version = \"${new}\"/" "$SERVER_TOML"
  else
    sed -i "0,/^version = \"${current}\"/s//version = \"${new}\"/" "$SERVER_TOML"
  fi
  ( cd "$REPO_ROOT" && cargo update -p orca --precise "$new" 2>/dev/null \
                       || cargo update -p orca 2>/dev/null || true )
}

# Compute next RC version from latest stable tag.
# In : $1 = patch|minor|major
# Out: "STABLE_VERSION RC_VERSION PREVIOUS_STABLE" (space-separated)
compute_rc_version() {
  local bump="$1"
  cd "$REPO_ROOT"
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
  if [ -z "$latest_rc" ]; then n=1; else n=${latest_rc##*rc.}; n=$((n+1)); fi
  echo "$next_stable" "${next_stable}-rc.${n}" "$latest_stable"
}

# ── checks ──────────────────────────────────────────────────────────────────

run_release_checks() {
  cd "$REPO_ROOT"
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

# ── frontend ────────────────────────────────────────────────────────────────

# Build the SvelteKit dist. Idempotent — `npm ci` is the only slow step and
# it's a no-op if package-lock hasn't changed. Both local + CI call this once
# before the per-target rust builds.
build_frontend() {
  log "building frontend (shared across all targets)"
  ( cd "$REPO_ROOT/projects/frontend" && npm ci && npm run build )
}

# ── per-target rust build ───────────────────────────────────────────────────

# Compile one target. Produces target/<target>/release/orca.
# Used by both release path (followed by stage_target_asset) and dev path
# (scripts/build-host.sh, invoked by `make build` — no staging needed).
#
# Args: $1 = target triple, $2 = cargo -j value
cargo_build_target() {
  local target="$1" jobs="$2"
  cd "$REPO_ROOT"

  local features_args=()
  [ -n "$RELEASE_FEATURES" ] && features_args=(--features "$RELEASE_FEATURES")

  log "building ${target} (profile=${RELEASE_PROFILE}, cargo -j${jobs}${RELEASE_FEATURES:+, features=$RELEASE_FEATURES})"
  if [ "$target" = "$(host_target)" ]; then
    cargo build --profile "$RELEASE_PROFILE" --jobs "$jobs" "${features_args[@]}" \
      --target "$target" --manifest-path "$SERVER_TOML"
  else
    cargo zigbuild --profile "$RELEASE_PROFILE" --jobs "$jobs" "${features_args[@]}" \
      --target "$target" --manifest-path "$SERVER_TOML"
  fi
}

# Copy the compiled binary into dist-release/ and write its sha256. Only
# called on the release path — `make build` skips this.
stage_target_asset() {
  local target="$1" asset="orca-${target}"
  cd "$REPO_ROOT"
  mkdir -p "$DIST_DIR"
  cp "target/${target}/${RELEASE_PROFILE}/orca" "${DIST_DIR}/${asset}"
  ( cd "$DIST_DIR" && shasum -a 256 "$asset" > "${asset}.sha256" )
}

# Compile + stage. The unit of work for one matrix runner in CI and for one
# slot in the local parallel build pool.
build_one_target() {
  cargo_build_target "$1" "$2"
  stage_target_asset "$1"
}

# Build many targets in parallel chunks. Local-only — CI uses matrix instead.
# Args: target1 target2 ...
build_orca_targets() {
  local targets=("$@")
  [ "${#targets[@]}" -gt 0 ] || die "build_orca_targets: no targets given"

  mkdir -p "$DIST_DIR"
  rm -f "$DIST_DIR"/orca-* "$DIST_DIR"/*.sha256

  local parallel jobs
  parallel=$(release_parallel_targets "${#targets[@]}")
  jobs=$(release_cargo_jobs "$parallel")
  log "building ${#targets[@]} targets — ${parallel} in parallel, cargo -j${jobs} each"

  # Chunked parallelism: bash 3.2 has no `wait -n`. Spawn $parallel jobs,
  # wait for all to finish, then start the next chunk. Per-target output is
  # tee'd to a logfile AND the console so progress is visible.
  local i=0
  while [ $i -lt ${#targets[@]} ]; do
    local pids=() chunk=()
    local j=0
    while [ $j -lt $parallel ] && [ $i -lt ${#targets[@]} ]; do
      local t="${targets[$i]}"
      chunk+=("$t")
      ( build_one_target "$t" "$jobs" 2>&1 | sed "s|^|[${t}] |" ) &
      pids+=($!)
      i=$((i+1)); j=$((j+1))
    done
    local failed=0 pid
    for pid in "${pids[@]}"; do
      wait "$pid" || failed=1
    done
    [ $failed -eq 0 ] || die "target build(s) failed in chunk: ${chunk[*]}"
  done

  ls -lh "$DIST_DIR"/
}

# Print asset paths for `gh release create`. Args: target1 target2 ...
release_asset_paths() {
  local t
  for t in "$@"; do
    echo "${DIST_DIR}/orca-${t}"
    echo "${DIST_DIR}/orca-${t}.sha256"
  done
}

# ── shared bump-then-build (the function the version bug lived in) ──────────

# Bump Cargo.toml to $1, refresh Cargo.lock, build frontend, build every
# target in $2..$N (parallel). Caller must set RB_CARGO=1 if it wants
# rollback on subsequent failure.
#
# CARGO_PKG_VERSION is baked in at compile time, so the bump MUST precede the
# build. Shared between rc and promote — fix once, fixed everywhere.
bump_and_build() {
  local new="$1"; shift
  local targets=("$@")
  [ "${#targets[@]}" -gt 0 ] || mapfile_to_array targets default_targets
  log "bumping ${SERVER_TOML} → ${new}"
  write_cargo_version "$new"
  build_frontend
  build_orca_targets "${targets[@]}"
}

# Bash-3.2-safe replacement for `mapfile`. Reads stdin of $2 into array $1.
# Usage: mapfile_to_array arr_name producer_fn_or_cmd
mapfile_to_array() {
  local _name="$1"; shift
  local _line
  eval "$_name=()"
  while IFS= read -r _line; do
    eval "$_name+=(\"\$_line\")"
  done < <("$@")
}

# ── changelog ───────────────────────────────────────────────────────────────

# Generate /tmp/orca-changelog.md.
# Args: $1=previous_stable_tag $2=new_tag $3=rc|stable $4=optional_extra_notes
#       $5..=target list (for install snippet)
generate_changelog() {
  set +o pipefail
  local prev="$1" new="$2" kind="$3" notes="${4:-}"; shift 4 || true
  local targets=("$@")
  [ "${#targets[@]}" -gt 0 ] || mapfile_to_array targets default_targets

  cd "$REPO_ROOT"
  local range commits repo
  if [ "$prev" = "v0.0.0" ]; then range="HEAD"; else range="${prev}..HEAD"; fi
  commits=$(git log "$range" --pretty=format:"%s" | grep -v '^chore: release v' | head -100)
  repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)

  _section() {
    local title="$1"; shift
    local items=""
    local prefix
    for prefix in "$@"; do
      items="$items$(echo "$commits" | grep -i "^${prefix}[:(]" | sed 's/^/- /' || true)"$'\n'
    done
    [ -n "$(echo "$items" | tr -d '[:space:]')" ] && printf "### %s\n%s\n" "$title" "$items"
    return 0
  }

  {
    [ -n "$notes" ] && printf "%s\n\n---\n\n" "$notes"
    if [ "$kind" = "rc" ]; then
      printf "> **Pre-release** \`rc\` — pending stable promotion.\n\n"
    else
      printf "> Promoted from RC.\n\n"
    fi
    printf "## What's Changed\n\n"
    _section 'Features'    feat feature
    _section 'Bug Fixes'   fix bug
    _section 'Performance' perf
    _section 'Refactoring' refactor refact
    _section 'Build / CI'  build ci chore
    _section 'Docs'        docs
    printf "\n## Installation\n\nOne-liner (auto-detects OS/arch, verifies sha256):\n\n\`\`\`sh\n"
    printf "curl -fsSL https://github.com/%s/releases/download/%s/install.sh | sh -s -- --version %s" "$repo" "$new" "$new"
    [ "$kind" = "rc" ] && printf " --prerelease"
    printf "\n\`\`\`\n\nSupported targets: %s\n\n" "${targets[*]}"
    printf "**Full diff:** [%s → %s](https://github.com/%s/compare/%s...%s)\n" "$prev" "$new" "$repo" "$prev" "$new"
  } > /tmp/orca-changelog.md
  set -o pipefail
}

# Prepend the current /tmp/orca-changelog.md into CHANGELOG.md under a
# Keep-A-Changelog heading. RC releases are intentionally excluded (their
# per-RC notes live in the GitHub release body only); stable promotions get
# a full entry. Pass --include-rc to override.
#
# Args: $1=tag (e.g. "v0.0.4") [$2=--include-rc]
prepend_changelog() {
  local tag="$1" include_rc=0
  [ "${2:-}" = "--include-rc" ] && include_rc=1

  # Skip RC tags unless explicitly asked
  if echo "$tag" | grep -qE '\-(rc|beta|alpha)\.' && [ "$include_rc" = "0" ]; then
    return 0
  fi

  [ -f /tmp/orca-changelog.md ] || { log "WARN: /tmp/orca-changelog.md missing, skipping CHANGELOG update"; return 0; }

  local date; date=$(date -u '+%Y-%m-%d')
  local dest="${REPO_ROOT}/CHANGELOG.md"
  local tmp; tmp=$(mktemp)

  # Write the new entry header + body into tmp
  {
    printf "## [%s] — %s\n\n" "$tag" "$date"
    cat /tmp/orca-changelog.md
    printf "\n---\n\n"
  } > "$tmp"

  # Prepend to existing CHANGELOG.md (create if absent)
  if [ -f "$dest" ]; then
    cat "$dest" >> "$tmp"
  fi
  mv "$tmp" "$dest"

  log "CHANGELOG.md updated for $tag"
}
