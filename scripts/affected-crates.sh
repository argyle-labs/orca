#!/usr/bin/env bash
# Compute the set of workspace crates affected by a change, for selective CI
# testing (#536). Prints EITHER the literal token `ALL` (test the whole
# workspace) OR a newline-separated list of crate names to test.
#
# The affected set = the changed crates PLUS the transitive closure of every
# workspace crate that (directly or indirectly) depends on them — so a change to
# a leaf crate still retests everything downstream that could break.
#
# SAFETY: this gates which tests run, so it FAILS OPEN. Any condition we can't
# reason about precisely -> print ALL and let the caller run the full suite:
#   - diff range can't be resolved
#   - a build-graph-wide file changed (root Cargo.toml/lock, CI, toolchain,
#     nextest/cargo config, the workspace-wide scripts)
#   - a changed file doesn't map to a known workspace crate
#
# Usage: affected-crates.sh <git-diff-range>
#   e.g. affected-crates.sh "origin/main...HEAD"
set -euo pipefail

range="${1:-}"
say_all() { echo "ALL"; exit 0; }

[ -z "$range" ] && say_all
command -v jq >/dev/null 2>&1 || say_all

changed="$(git diff --name-only "$range" 2>/dev/null)" || say_all
[ -z "$changed" ] && { echo ""; exit 0; }   # nothing changed -> test nothing

# ── Fail-open on build-graph-wide changes ────────────────────────────────────
while IFS= read -r f; do
  [ -z "$f" ] && continue
  case "$f" in
    Cargo.toml|Cargo.lock) say_all ;;                 # root manifest / lockfile
    .github/workflows/*) say_all ;;                   # CI itself
    rust-toolchain|rust-toolchain.toml) say_all ;;    # toolchain pin
    .config/nextest.toml) say_all ;;                  # test-runner config
    .cargo/*) say_all ;;                              # cargo config
    scripts/affected-crates.sh) say_all ;;            # this selector changed
  esac
done <<< "$changed"

# ── Map workspace crates -> their manifest directory (relative, no trailing /) ─
# meta: one JSON doc with the package list + each package's manifest dir.
meta="$(cargo metadata --format-version 1 --no-deps 2>/dev/null)" || say_all
root="$(pwd)"

# name<TAB>reldir for every workspace member, longest reldir first so nested
# crates match before their parents.
mapfile -t crate_dirs < <(
  echo "$meta" | jq -r --arg root "$root/" '
    .packages[]
    | [.name, (.manifest_path | rtrimstr("/Cargo.toml") | ltrimstr($root))]
    | @tsv' | awk -F'\t' '{ print length($2)"\t"$0 }' | sort -rn | cut -f2-
)

# ── Determine the changed crates by matching each file to its owning crate ────
declare -A changed_crates=()
while IFS= read -r f; do
  [ -z "$f" ] && continue
  owner=""
  for line in "${crate_dirs[@]}"; do
    name="${line%%$'\t'*}"; dir="${line#*$'\t'}"
    if [ "$dir" = "." ]; then
      owner="$name"   # a root-level crate (unlikely) — weakest match, keep looking
    elif [ "$f" = "$dir" ] || [ "${f#"$dir"/}" != "$f" ]; then
      owner="$name"; break
    fi
  done
  # A changed file under no known crate (docs/, top-level misc) -> fail open,
  # because we can't prove it doesn't affect the build.
  [ -z "$owner" ] && say_all
  changed_crates["$owner"]=1
done <<< "$changed"

[ "${#changed_crates[@]}" -eq 0 ] && { echo ""; exit 0; }

# ── Build the workspace-internal reverse-dependency edges: rdeps[dep] += pkg ──
# (pkg depends on dep, both workspace members) then BFS the closure.
rdeps_json="$(echo "$meta" | jq -c '
  [.packages[].name] as $ws
  | reduce .packages[] as $p ({};
      reduce ($p.dependencies[].name | select(. as $d | $ws | index($d))) as $d (.;
        .[$d] += [$p.name]))')"

# Seed the queue with the changed crates; BFS over rdeps to the fixpoint.
declare -A affected=()
queue=("${!changed_crates[@]}")
while [ "${#queue[@]}" -gt 0 ]; do
  cur="${queue[0]}"; queue=("${queue[@]:1}")
  [ -n "${affected[$cur]:-}" ] && continue
  affected["$cur"]=1
  # dependents of cur
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    [ -z "${affected[$dep]:-}" ] && queue+=("$dep")
  done < <(echo "$rdeps_json" | jq -r --arg c "$cur" '.[$c] // [] | .[]')
done

# Emit the affected crate names, sorted + unique.
for k in "${!affected[@]}"; do echo "$k"; done | sort -u
