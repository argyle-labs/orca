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

# Extra cargo features for release builds. The `ui` feature is on by default
# (see projects/server/Cargo.toml `[features] default = ["ui"]`) so the
# embedded frontend ships in every release without explicit opt-in. Use this
# knob to add other optional features (e.g. `pdf`, `php-ast`). For a true
# headless build, pass `--no-default-features` directly via cargo.
: "${RELEASE_FEATURES:=}"

# Cargo profile. Defaults to `release` (fat LTO, codegen-units=1 — slow build,
# fast binary; required for shipped releases). `make build` overrides to
# `release-fast` (thin LTO, 16 codegen units — uses every core, slightly
# larger/slower binary, fine for dev).
: "${RELEASE_PROFILE:=release}"

# ── target sets ─────────────────────────────────────────────────────────────

# Full catalog. Kept intact so opt-in builds (or a future fleet that needs
# them) can request these without re-adding entries.
#
# All Linux targets are cross-compiled via cargo-zigbuild from any host.
LINUX_TARGETS_ALL=(
  x86_64-unknown-linux-gnu
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-gnu
  aarch64-unknown-linux-musl
)
# macOS targets require a macOS host (no osxcross).
MAC_TARGETS_ALL=(aarch64-apple-darwin x86_64-apple-darwin)

# Subset actually deployed today (2026-06-03):
#   - aarch64-apple-darwin  → hotel (M3 Max workstation)
#   - x86_64-unknown-linux-gnu  → alpha, echo (Unraid), delta, golf, foxtrot (Proxmox/Debian)
#   - x86_64-unknown-linux-musl → bravo, charlie (Alpine)
# Skipped: aarch64-linux-{gnu,musl}, x86_64-apple-darwin (no host on the fleet).
# When a new host arch joins the fleet, add it here. To temporarily build
# everything in the catalog, set RELEASE_TARGETS_ALL=1 in the environment.
LINUX_TARGETS_ACTIVE=(
  x86_64-unknown-linux-gnu
  x86_64-unknown-linux-musl
)
MAC_TARGETS_ACTIVE=(aarch64-apple-darwin)

# Back-compat alias — older callers may still reference LINUX_TARGETS.
LINUX_TARGETS=("${LINUX_TARGETS_ALL[@]}")

# Default target list for the current host. Callers can override by setting
# RELEASE_TARGETS="t1 t2 ..." in the environment, or RELEASE_TARGETS_ALL=1 to
# fall back to every catalog target, or by passing the list to
# build_orca_targets directly.
default_targets() {
  local out=()
  local mac_set=("${MAC_TARGETS_ACTIVE[@]}")
  local linux_set=("${LINUX_TARGETS_ACTIVE[@]}")
  if [ "${RELEASE_TARGETS_ALL:-0}" = "1" ]; then
    mac_set=("${MAC_TARGETS_ALL[@]}")
    linux_set=("${LINUX_TARGETS_ALL[@]}")
  fi
  case "$(uname -s)" in
    Darwin) out=("${mac_set[@]}" "${linux_set[@]}") ;;
    *)      out=("${linux_set[@]}") ;;
  esac
  printf '%s\n' "${out[@]}"
}

host_target() {
  # Prefer rustc so we agree with cargo's notion of host. Fall back to
  # uname so this works on CI runners that haven't installed a toolchain
  # (the package-native job downloads pre-built binaries — no rustc).
  if command -v rustc >/dev/null 2>&1; then
    rustc -vV | awk '/^host:/ {print $2}'
    return
  fi
  local os arch
  case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux)  os=unknown-linux-gnu ;;
    *) die "host_target: unsupported OS $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) die "host_target: unsupported arch $(uname -m)" ;;
  esac
  echo "${arch}-${os}"
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
  local num_targets="$1"
  local parallel
  if [ -n "${RELEASE_PARALLEL_TARGETS:-}" ]; then
    parallel="$RELEASE_PARALLEL_TARGETS"
  else
    # Default to 1: parallel cargo builds fight over the package cache and
    # artifact directory locks. Set RELEASE_PARALLEL_TARGETS to override.
    parallel=1
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
  grep '^version' "${REPO_ROOT}/Cargo.toml" | head -1 | sed 's/version = "\(.*\)"/\1/'
}

write_cargo_version() {
  local new="$1"
  local workspace_toml="${REPO_ROOT}/Cargo.toml"
  # Replace the first `version = "..."` line in the workspace root.
  # Don't match on the old value — avoids regex-escaping pre-release strings.
  if [ "$(uname -s)" = "Darwin" ]; then
    sed -i '' 's/^version = ".*"/version = "'"$new"'"/' "$workspace_toml"
  else
    sed -i 's/^version = ".*"/version = "'"$new"'"/' "$workspace_toml"
  fi
  log "workspace version → $new ($(grep '^version' "$workspace_toml" | head -1))"
  # Keep the README release badge in sync. orca is a private repo, so a live
  # shields.io github-release query returns "repo not found"; a static badge is
  # the only kind that renders. shields static labels escape "-" as "--".
  local readme="${REPO_ROOT}/README.md"
  if [ -f "$readme" ]; then
    local enc="v$(printf '%s' "$new" | sed 's/-/--/g')"
    if [ "$(uname -s)" = "Darwin" ]; then
      sed -i '' 's#badge/release-[^)]*-blue#badge/release-'"$enc"'-blue#' "$readme"
    else
      sed -i 's#badge/release-[^)]*-blue#badge/release-'"$enc"'-blue#' "$readme"
    fi
    log "README release badge → $enc"
  fi
  # Regenerate Cargo.lock from the updated Cargo.toml.
  ( cd "$REPO_ROOT" && cargo update -p orca 2>/dev/null || true )
  # Bake the release version verbatim into the binary. build.rs reads this
  # env var first; without it, build.rs falls back to `<cargo>-dev+g<sha>.dirty`
  # because the working tree is dirty (Cargo.toml just changed) and the tag
  # doesn't exist yet (created after the build). All subsequent cargo
  # invocations in this shell — local `make release rc` AND the CI composite
  # action that `source`s this lib — inherit it.
  export ORCA_RELEASE_VERSION="$new"
  log "exported ORCA_RELEASE_VERSION=$new for build.rs"
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
  _sdk_tree=$(cargo tree -p orca-sdk 2>&1 || true)
  if echo "$_sdk_tree" | grep -qE "orca-server|orca-commands|orca-conversation|orca-agents|orca-scanner|rust-embed"; then
    die "server-only crate found in orca-sdk dependency tree"
  fi
  # Tests run in dev profile — release optimisation gives no signal here and
  # ~3-5x's the compile time. The release-artifact build that follows is the
  # only thing that needs `--release`.
  log "cargo test (workspace, dev profile)"
  if command -v cargo-nextest >/dev/null 2>&1; then
    cargo nextest run --workspace --no-fail-fast
  else
    cargo test --workspace --no-fail-fast
  fi
  log "doctests (dev profile)"
  cargo test --doc --workspace --no-fail-fast
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
  local target="$1"
  local jobs="$2"
  cd "$REPO_ROOT"

  local features_args=()
  [ -n "${RELEASE_NO_DEFAULT_FEATURES:-}" ] && features_args+=(--no-default-features)
  [ -n "$RELEASE_FEATURES" ] && features_args+=(--features "$RELEASE_FEATURES")

  log "building ${target} (profile=${RELEASE_PROFILE}, cargo -j${jobs}${RELEASE_NO_DEFAULT_FEATURES:+, no-default-features}${RELEASE_FEATURES:+, features=$RELEASE_FEATURES})"

  # Apple targets: native Apple clang (not zigbuild — zigbuild routes C through
  # zig which breaks SQLCipher's cc-rs on Apple silicon). Pin deployment target
  # to 11.0 so macOS beta host versions don't leak into -mmacosx-version-min.
  # Shared target/ dir is safe here because builds run sequentially (parallel=1).
  case "$target" in
    *-apple-darwin)
      MACOSX_DEPLOYMENT_TARGET=11.0 cargo build \
        --profile "$RELEASE_PROFILE" --jobs "$jobs" ${features_args[@]+"${features_args[@]}"} \
        --target "$target" --manifest-path "$SERVER_TOML"
      ;;
    *)
      # Linux and other non-Apple targets: zigbuild for cross-compilation.
      # Reduce codegen-units to 4 for cross-compile: the Zig linker opens all
      # object files simultaneously and hits EMFILE (ProcessFdQuotaExceeded)
      # with the default 16 units (~2800 objects). 4 units ~= 700 objects,
      # well under macOS kern.maxfilesperproc. Native macOS builds keep 16.
      CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4 \
        cargo zigbuild --profile "$RELEASE_PROFILE" --jobs "$jobs" ${features_args[@]+"${features_args[@]}"} \
        --target "$target" --manifest-path "$SERVER_TOML"
      ;;
  esac
}

# Copy the compiled binary into dist-release/ and write its sha256. Only
# called on the release path — `make build` skips this.
stage_target_asset() {
  local target="$1"
  local version
  version="$(current_cargo_version)"
  local asset="orca-${version}-${target}"
  cd "$REPO_ROOT"
  mkdir -p "$DIST_DIR"
  cp "target/${target}/${RELEASE_PROFILE}/orca" "${DIST_DIR}/${asset}"
  # Ad-hoc sign Darwin assets BEFORE hashing so the published sha256 matches
  # what install.sh writes to disk. Without the signature, macOS Gatekeeper
  # SIGKILLs the binary on first launch — even when invoked from launchd.
  # codesign only runs on the build host's mac; cross-built darwin assets
  # produced on Linux get signed by install.sh on the target instead.
  case "$target" in
    *-apple-darwin)
      if [ "$(uname -s)" = "Darwin" ]; then
        codesign --force --sign - "${DIST_DIR}/${asset}" 2>/dev/null \
          || log "warn: codesign failed for ${asset} — install.sh will retry on target"
      fi
      ;;
  esac
  ( cd "$DIST_DIR" && shasum -a 256 "$asset" > "${asset}.sha256" )

  # Legacy unversioned alias (`orca-<target>` + `.sha256`). Hosts on a binary
  # built before commands/update.rs learned the versioned naming look up the
  # unversioned form. Keep shipping both until the entire fleet is past v0.0.4;
  # remove this block after that point.
  local legacy="orca-${target}"
  cp "${DIST_DIR}/${asset}" "${DIST_DIR}/${legacy}"
  ( cd "$DIST_DIR" && shasum -a 256 "$legacy" > "${legacy}.sha256" )
}

# Compile + stage. The unit of work for one matrix runner in CI and for one
# slot in the local parallel build pool.
build_one_target() {
  cargo_build_target "$1" "$2"
  stage_target_asset "$1"
}

# ── install (single source for Make + CI smoke installs) ────────────────────
#
# Copy a built orca binary into a destination path as a real file.
#
# Rules — same on every runner (local make, GH Actions, Gitea Actions):
#   1. If the dest is a symlink, remove it first. cp -f would otherwise follow
#      the link and overwrite the build artifact in place — the exact drift
#      that caused mystery "stale binary" bugs.
#   2. Skip the copy + codesign if the bytes already match (idempotent).
#   3. On macOS, ad-hoc codesign so Gatekeeper accepts launchd execs.
#   4. Never call `daemon install` from here — composing that is the caller's
#      job (Make does it, CI doesn't).
#
# Args:
#   $1 = source binary path  (e.g. target/<triple>/release/orca)
#   $2 = destination path    (e.g. ~/.local/bin/orca)
install_orca_binary() {
  local src="$1"
  local dest="$2"
  [ -n "$src" ]  || die "install_orca_binary: source path required"
  [ -n "$dest" ] || die "install_orca_binary: destination path required"
  [ -f "$src" ]  || die "install_orca_binary: source not found: $src"

  mkdir -p "$(dirname "$dest")"

  if [ -L "$dest" ]; then
    log "removing stale symlink at ${dest}"
    rm -f "$dest"
  fi

  if [ -f "$dest" ] && cmp -s "$src" "$dest"; then
    log "binary unchanged → ${dest}"
    return 0
  fi

  cp "$src" "$dest"
  chmod +x "$dest"

  if [ "$(uname -s)" = "Darwin" ]; then
    codesign --force --sign - "$dest" 2>/dev/null || true
  fi

  log "installed → ${dest}"
}

# Build many targets in parallel chunks. Local-only — CI uses matrix instead.
# Args: target1 target2 ...
build_orca_targets() {
  local targets=("$@")
  [ "${#targets[@]}" -gt 0 ] || die "build_orca_targets: no targets given"

  mkdir -p "$DIST_DIR"
  rm -rf "$DIST_DIR"/orca-* "$DIST_DIR"/*.sha256 "$DIST_DIR"/*.sha256.bak \
        "$DIST_DIR"/*.deb "$DIST_DIR"/*.rpm "$DIST_DIR"/*.pkg "$DIST_DIR"/*.rb \
        "$DIST_DIR"/orca.plg "$DIST_DIR"/APKBUILD "$DIST_DIR"/PKGBUILD

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
# Emits both the versioned and legacy unversioned aliases (see stage_target_asset
# for the transition rationale), then native-package artifacts produced by
# build_native_packages (skipped silently if none present).
release_asset_paths() {
  local version t
  version="$(current_cargo_version)"
  for t in "$@"; do
    echo "${DIST_DIR}/orca-${version}-${t}"
    echo "${DIST_DIR}/orca-${version}-${t}.sha256"
    echo "${DIST_DIR}/orca-${t}"
    echo "${DIST_DIR}/orca-${t}.sha256"
  done
  # Native packages live next to the binaries in dist-release/. Globs
  # are safe — missing matches expand to nothing under `nullglob`.
  local f
  shopt -s nullglob
  for f in \
    "$DIST_DIR"/*.deb \
    "$DIST_DIR"/*.rpm \
    "$DIST_DIR"/*.pkg \
    "$DIST_DIR"/*.rb \
    "$DIST_DIR"/orca.plg \
    "$DIST_DIR"/APKBUILD \
    "$DIST_DIR"/PKGBUILD; do
    [ -e "$f" ] || continue
    echo "$f"
  done
  shopt -u nullglob
}

# Build every native installer format the host can produce, alongside the
# binaries in dist-release/. Mirrors the package-native matrix in
# .github/workflows/release.yml (see [[feedback-ci-makefile-parity]]).
#
# Each format runs `orca system build --format <fmt>` via the freshly built
# host-target binary; package.rs handles the per-format details. Formats
# whose external tool is missing (dpkg-deb, rpmbuild, pkgbuild) are skipped
# with a warning instead of failing the release — local hosts rarely have
# every packager installed.
build_native_packages() {
  local host
  host="$(host_target)"
  local runner="${DIST_DIR}/orca-${host}"
  [ -x "$runner" ] || die "build_native_packages: host binary ${runner} missing — run build_orca_targets first"

  # (format, target-arch, target-triple, required-tool) per matrix entry.
  # An empty required-tool means the format only writes source files.
  local rows=(
    "deb      x86_64  x86_64-unknown-linux-gnu   dpkg-deb"
    "deb      aarch64 aarch64-unknown-linux-gnu  dpkg-deb"
    "rpm      x86_64  x86_64-unknown-linux-gnu   rpmbuild"
    "rpm      aarch64 aarch64-unknown-linux-gnu  rpmbuild"
    "apk      x86_64  x86_64-unknown-linux-musl  "
    "pkgbuild x86_64  x86_64-unknown-linux-gnu   "
    "homebrew x86_64  x86_64-unknown-linux-gnu   "
    "pkg      x86_64  x86_64-apple-darwin        pkgbuild"
    "pkg      aarch64 aarch64-apple-darwin       pkgbuild"
    "plg      x86_64  x86_64-unknown-linux-gnu   "
  )

  local row fmt arch triple tool bin
  for row in "${rows[@]}"; do
    read -r fmt arch triple tool <<< "$row"
    bin="${DIST_DIR}/orca-${triple}"
    if [ ! -f "$bin" ]; then
      log "skip ${fmt}/${arch} — ${bin} not in build set"
      continue
    fi
    if [ -n "$tool" ] && ! command -v "$tool" >/dev/null 2>&1; then
      log "skip ${fmt}/${arch} — missing ${tool}"
      continue
    fi
    # rpmbuild cannot cross-build Linux rpms on a non-Linux host: even with
    # `--target <arch>`, macOS rpmbuild (Homebrew) has no matching platform in
    # its arch tables and dies with "No compatible architectures found for
    # build". Skip rpm on non-Linux hosts — build it on a Linux runner/CI when
    # Fedora/RHEL assets are actually needed. deb cross-builds fine here.
    if [ "$fmt" = "rpm" ] && [ "$(uname -s)" != "Linux" ]; then
      log "skip ${fmt}/${arch} — rpmbuild cannot cross-build on $(uname -s); build on a Linux host"
      continue
    fi
    log "package ${fmt}/${arch} (binary: orca-${triple})"
    "$runner" system build \
      --format "$fmt" \
      --binary "$bin" \
      --arch "$arch" \
      --out-dir "$DIST_DIR" \
      || die "package ${fmt}/${arch} failed"
  done
}

# ── rc cleanup ──────────────────────────────────────────────────────────────
#
# Delete every published RC release + matching git tag for a given stable
# version. Used after `promote` so the GitHub releases page doesn't
# accumulate dead `v0.0.3-rc.1 ... rc.N` entries forever.
#
# Args: $1 = stable version string WITHOUT the leading `v` (e.g. "0.0.3").
#       $2 = optional "--dry-run" — print actions but make no changes.
#
# Idempotent: missing releases/tags are skipped silently.
cleanup_rcs() {
  local stable="$1"; local dry="${2:-}"
  [ -n "$stable" ] || die "cleanup_rcs: stable version required (e.g. 0.0.3)"
  cd "$REPO_ROOT"

  local repo; repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
  local pattern="v${stable}-rc."
  log "cleaning up RC releases matching ${pattern}* on ${repo}"

  # Releases: ask GH for everything, filter by prefix locally. Avoids the
  # 30-item default `gh release list` page in case of long RC trains.
  local rc_tags
  rc_tags=$(gh release list --repo "$repo" --limit 200 --json tagName \
    --jq ".[] | .tagName | select(startswith(\"${pattern}\"))" || true)

  if [ -z "$rc_tags" ]; then
    log "no RC releases for v${stable} — nothing to clean"
  else
    local tag
    while IFS= read -r tag; do
      [ -z "$tag" ] && continue
      if [ "$dry" = "--dry-run" ]; then
        log "  [dry-run] would delete release + remote tag ${tag}"
      else
        log "  deleting release + remote tag ${tag}"
        # --cleanup-tag also removes the matching remote git tag in one call.
        gh release delete "$tag" --repo "$repo" --cleanup-tag --yes || \
          log "    warn: gh release delete ${tag} failed (skipping)"
      fi
    done <<EOF
$rc_tags
EOF
  fi

  # Local tags (separate from remote tags — `gh release delete --cleanup-tag`
  # only touches the remote). Walk every local tag matching the prefix.
  local local_tags
  local_tags=$(git tag -l "${pattern}*")
  if [ -n "$local_tags" ]; then
    while IFS= read -r tag; do
      [ -z "$tag" ] && continue
      if [ "$dry" = "--dry-run" ]; then
        log "  [dry-run] would delete local tag ${tag}"
      else
        log "  deleting local tag ${tag}"
        git tag -d "$tag" >/dev/null || true
      fi
    done <<EOF
$local_tags
EOF
  fi
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
  build_native_packages
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
  local prev="$1"
  local new="$2"
  local kind="$3"
  local notes="${4:-}"
  shift 4 || true
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
  local tag="$1"
  local include_rc=0
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
