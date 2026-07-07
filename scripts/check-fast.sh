#!/usr/bin/env bash
# Fast inner-loop check: fmt + clippy + tests, scoped to crates changed since
# the base ref (default origin/main). Walks reverse-deps one level so a touched
# API gets all direct consumers retested. Skips the full `cargo llvm-cov` gate
# — CI runs that.
#
# Usage:
#   ./scripts/check-fast.sh            # diff against origin/main
#   ./scripts/check-fast.sh main       # diff against local main
#   ./scripts/check-fast.sh HEAD~3     # last 3 commits
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

BASE="${1:-origin/main}"

# Files that touch the Rust build graph: any .rs source + Cargo.toml manifests
changed_files=$(
  git diff --name-only "$BASE"...HEAD -- '*.rs' Cargo.toml '**/Cargo.toml' 2>/dev/null || true
)

if [ -z "$changed_files" ]; then
  echo "==> no rust changes since $BASE — nothing to do"
  exit 0
fi

# Map each changed file to its nearest Cargo.toml, then to that crate's name.
direct_crates=$(
  echo "$changed_files" | while read -r f; do
    [ -z "$f" ] && continue
    d="$(dirname "$f")"
    while [ "$d" != "." ] && [ ! -f "$d/Cargo.toml" ]; do
      d="$(dirname "$d")"
    done
    if [ -f "$d/Cargo.toml" ] && grep -q '^name' "$d/Cargo.toml"; then
      grep -m1 '^name' "$d/Cargo.toml" | sed 's/name = "\(.*\)"/\1/'
    fi
  done | sort -u
)

if [ -z "$direct_crates" ]; then
  echo "==> no workspace crates touched — nothing to do"
  exit 0
fi

# Expand to direct reverse-deps: anyone who consumes a changed crate gets
# retested. `cargo tree --invert` lists `<consumer> <version>` per line.
expanded=$(
  for c in $direct_crates; do
    echo "$c"
    cargo tree --invert -p "$c" --depth 1 --prefix none --workspace 2>/dev/null \
      | awk '{print $1}' \
      | grep -v '^$' || true
  done | sort -u
)

echo "==> changed crates: $(echo "$direct_crates" | tr '\n' ' ')"
echo "==> testing (with direct rdeps): $(echo "$expanded" | tr '\n' ' ')"

# Build -p args
p_args=()
for c in $expanded; do
  p_args+=(-p "$c")
done

echo "==> rustfmt (changed files only)"
echo "$changed_files" | grep '\.rs$' | xargs -I{} -n1 rustfmt --check {} || {
  echo "rustfmt would change files — run 'cargo fmt --all'"
  exit 1
}

echo "==> clippy"
RUSTFLAGS="-D warnings" cargo clippy "${p_args[@]}" --tests -- -D warnings

echo "==> cargo test"
cargo test "${p_args[@]}" --lib --bins --tests

echo "==> fast check passed"
