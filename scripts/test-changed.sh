#!/usr/bin/env bash
# Run tests only for crates (and frontend) whose sources changed vs a base ref.
#
# Usage:
#   scripts/test-changed.sh                  # diff vs main + working tree
#   BASE=origin/main scripts/test-changed.sh # diff vs a different ref
#   scripts/test-changed.sh --include-deps   # also run reverse-dep crates
#
# Strategy:
#   1. Collect changed paths from `git diff --name-only $BASE` plus uncommitted
#      changes (`git status --porcelain`).
#   2. For each path, walk up to the nearest Cargo.toml that declares a
#      [package]; record the package name.
#   3. If --include-deps, expand the set via `cargo tree --invert -p <pkg>` so
#      reverse dependents are tested too.
#   4. Run `cargo nextest run` with `-p` flags. If any `projects/frontend/**`
#      changed, also run vitest.
#
# Bails out to a full workspace test if it can't determine the set safely
# (e.g. workspace Cargo.toml or a root config touched).

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

BASE="${BASE:-main}"
INCLUDE_DEPS=0
for arg in "$@"; do
  case "$arg" in
    --include-deps) INCLUDE_DEPS=1 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

# Files changed since BASE plus anything dirty in the working tree.
mapfile -t CHANGED < <({
  git diff --name-only "$BASE"... 2>/dev/null || true
  git status --porcelain | awk '{ print $NF }'
} | sort -u | sed '/^$/d')

if [[ ${#CHANGED[@]} -eq 0 ]]; then
  echo "no changes vs $BASE — nothing to test"
  exit 0
fi

# Tripwires: if these change, fall back to a full run.
for f in "${CHANGED[@]}"; do
  case "$f" in
    Cargo.toml|Cargo.lock|rust-toolchain*|.cargo/*|Makefile|deny.toml)
      echo "→ tripwire ($f) — running full workspace test"
      exec make test
      ;;
  esac
done

frontend_changed=0
declare -A PKGS=()

# Map a file to its owning package by walking up looking for a Cargo.toml
# with a [package] section. Returns empty if none found (e.g. doc-only edits).
owner_pkg() {
  local p="$1" dir
  dir="$(dirname "$p")"
  while [[ "$dir" != "." && "$dir" != "/" ]]; do
    if [[ -f "$dir/Cargo.toml" ]] && grep -q '^\[package\]' "$dir/Cargo.toml"; then
      awk '/^\[package\]/{p=1; next} p && /^name *= */{ gsub(/[" ]/,""); sub(/name=/,""); print; exit }' "$dir/Cargo.toml"
      return
    fi
    dir="$(dirname "$dir")"
  done
}

for f in "${CHANGED[@]}"; do
  [[ -e "$f" || -e "$(dirname "$f")" ]] || continue
  case "$f" in
    projects/frontend/*) frontend_changed=1 ;;
  esac
  name="$(owner_pkg "$f" || true)"
  [[ -n "$name" ]] && PKGS["$name"]=1
done

if [[ $INCLUDE_DEPS -eq 1 && ${#PKGS[@]} -gt 0 ]]; then
  for p in "${!PKGS[@]}"; do
    while IFS= read -r dep; do
      [[ -n "$dep" ]] && PKGS["$dep"]=1
    done < <(cargo tree --invert -p "$p" --depth 1 --prefix none 2>/dev/null \
              | awk '{ print $1 }' | sed '/^$/d' | sort -u)
  done
fi

if [[ $frontend_changed -eq 1 ]]; then
  echo "→ vitest (frontend changed)"
  (cd projects/frontend && npx vitest run)
fi

if [[ ${#PKGS[@]} -eq 0 ]]; then
  echo "no Rust packages changed"
  exit 0
fi

ARGS=()
for p in "${!PKGS[@]}"; do
  ARGS+=(-p "$p")
done

echo "→ cargo nextest for: ${!PKGS[*]}"
TARGET_DIR_NATIVE="${TARGET_DIR_NATIVE:-target}"
CARGO_TARGET_DIR="$TARGET_DIR_NATIVE" \
  cargo nextest run --no-fail-fast "${ARGS[@]}"
