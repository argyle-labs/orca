#!/usr/bin/env bash
# Keeps a cached target/ honest across CI runs (see the test-rust job in
# .github/workflows/ci.yml).
#
#   restore  Run right after the cache restore, from the repo root. Checkout
#            stamps every file "now", so cargo would rebuild every workspace
#            crate. If the restored cache is complete (sentinel present), files
#            whose blob matches the manifest are backdated so cargo trusts their
#            fingerprints; changed files keep "now". A path added or removed
#            since the manifest keeps "now" on its whole crate (nearest tracked
#            Cargo.toml): include_dir! and similar macros embed directory
#            listings cargo does not track on stable.
#   prune    Run after a successful build: drops test executables in
#            target/debug/deps the current build did not produce (stale ones
#            would be fed to `llvm-cov report`) and per-run output.
#   record   Run last before save: writes the manifest, then the sentinel.
#
# Manifest = `git ls-files -s -z` of the tree the artifacts were built from.
set -euo pipefail

TARGET="${CARGO_TARGET_DIR:-target}"
CRATE=""
MANIFEST="$TARGET/.ci-src-manifest"
SENTINEL="$TARGET/.ci-cache-complete"
OLD=946684800 # 2000-01-01, older than any artifact

clean_run_output() {
  find "$TARGET" -name '*.profraw' -delete 2>/dev/null || true
  rm -rf "$TARGET/cargo-timings" "$TARGET/llvm-cov"
}

backdate() {
  [ "$#" -gt 0 ] || return 0
  printf '%s\0' "$@" | xargs -0 touch -c -h -d "@$OLD" --
}

cmd_restore() {
  clean_run_output
  if [ ! -f "$SENTINEL" ] || [ ! -f "$MANIFEST" ]; then
    echo "target cache: no complete cache restored; sources keep checkout mtimes"
    rm -f "$SENTINEL"
    return 0
  fi
  rm -f "$SENTINEL"

  declare -A old new
  local rec
  while IFS= read -r -d '' rec; do old["${rec#*$'\t'}"]="${rec%%$'\t'*}"; done <"$MANIFEST"
  while IFS= read -r -d '' rec; do new["${rec#*$'\t'}"]="${rec%%$'\t'*}"; done < <(git ls-files -s -z)

  declare -A crate_memo=() dirty=()
  local p c
  for p in "${!new[@]}"; do
    [ -n "${old[$p]+x}" ] && continue
    crate_of "$p"
    [ -n "$CRATE" ] && dirty["$CRATE"]=1
  done
  for p in "${!old[@]}"; do
    [ -n "${new[$p]+x}" ] && continue
    crate_of "$p"
    [ -n "$CRATE" ] && dirty["$CRATE"]=1
  done

  local -a keep=()
  local changed=0
  for p in "${!new[@]}"; do
    if [ "${old[$p]-}" != "${new[$p]}" ]; then
      changed=$((changed + 1))
      continue
    fi
    crate_of "$p"
    c="$CRATE"
    [ -n "${dirty[.]+x}" ] && continue
    [ -n "$c" ] && [ -n "${dirty[$c]+x}" ] && continue
    keep+=("$p")
  done
  backdate "${keep[@]+"${keep[@]}"}"
  # server/system build scripts rerun-if-changed on these; their output is
  # pinned by ORCA_RELEASE_VERSION in CI.
  touch -c -d "@$OLD" .git/HEAD .git/index

  echo "target cache: ${#keep[@]} unchanged backdated, $changed changed," \
    "crates with added/removed paths: ${!dirty[*]}"
}

# Sets CRATE to the nearest ancestor directory holding a tracked Cargo.toml,
# "." for the root only when it is a package, or "" for a path in no crate
# (scripts/, docs/, .github/). Reads the caller's `new`, `old` and
# `crate_memo` maps; memoised per directory.
crate_of() {
  local d="${1%/*}" start
  [ "$d" = "$1" ] && d="."
  start="$d"
  if [ -n "${crate_memo[$start]+x}" ]; then
    CRATE="${crate_memo[$start]}"
    return
  fi
  CRATE=""
  while [ "$d" != "." ]; do
    if [ -n "${new[$d/Cargo.toml]+x}" ] || [ -n "${old[$d/Cargo.toml]+x}" ]; then
      CRATE="$d"
      break
    fi
    case "$d" in
      */*) d="${d%/*}" ;;
      *) d="." ;;
    esac
  done
  if [ -z "$CRATE" ] && grep -q '^\[package\]' Cargo.toml 2>/dev/null; then
    CRATE="."
  fi
  crate_memo["$start"]="$CRATE"
}

# $1 = cargo JSON message stream of the build just run.
cmd_prune() {
  local messages="$1" deps="$TARGET/debug/deps"
  clean_run_output
  rm -rf "$TARGET/debug/incremental"
  [ -d "$deps" ] || return 0
  declare -A live=()
  local exe
  # Uplifted binaries are hardlinks of their deps/ copy, so match by inode.
  while IFS= read -r exe; do
    [ -n "$exe" ] && [ -e "$exe" ] && live["$(stat -c %i -- "$exe")"]=1
  done < <(grep -o '"executable":"[^"]*"' "$messages" | sed 's/^"executable":"//; s/"$//')
  if [ "${#live[@]}" -eq 0 ] && [ -n "$(find "$deps" -maxdepth 1 -type f -perm -u+x ! -name '*.*' -print -quit)" ]; then
    echo "::warning title=target cache not saved::cargo listed no executables in $1 but $deps has some; refusing to prune"
    return 3
  fi
  local f removed=0
  while IFS= read -r -d '' f; do
    if [ -z "${live[$(stat -c %i -- "$f")]+x}" ]; then
      rm -f -- "$f"
      removed=$((removed + 1))
    fi
  done < <(find "$deps" -maxdepth 1 -type f -perm -u+x ! -name '*.*' -print0)
  echo "target cache: pruned $removed stale executables from $deps"
}

cmd_record() {
  clean_run_output
  git ls-files -s -z >"$MANIFEST"
  : >"$SENTINEL"
}

case "${1:-}" in
  restore) cmd_restore ;;
  prune) cmd_prune "$2" ;;
  record) cmd_record ;;
  *)
    echo "usage: $0 restore | prune <cargo-json> | record" >&2
    exit 2
    ;;
esac
