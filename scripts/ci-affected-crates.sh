#!/usr/bin/env bash
# Maps changed paths (one per line on stdin, relative to the workspace root) to
# the workspace packages CI must test, and prints GITHUB_OUTPUT lines:
#
#   mode=all|partial|none
#   args=--workspace | -p a -p b ...
#   packages=a b ...
#   ignore_regex=<llvm-cov --ignore-filename-regex for untested members>
#
# A path maps to the member whose directory is its nearest ancestor; docs/**
# maps to `files`. The set is closed over reverse dependencies (normal, dev and
# build edges, any target), so a library change still tests every dependent. A
# partial run also tests ALWAYS_PACKAGES (not their dependents), whose tests
# walk the repo and docs.
# Other *.md files select only those. LICENSE* is ignored; mode=none means no
# Rust-relevant change. Fails open: any other path that maps to no member, or a
# path count that differs from CI_EXPECTED_COUNT, selects the whole workspace.
#
# CI_METADATA_JSON names a `cargo metadata --format-version 1 --no-deps` file
# to use instead of running cargo.
set -euo pipefail

ALWAYS_PACKAGES="${CI_ALWAYS_PACKAGES-inventory-tests plugin-toolkit}"
DOCS_PACKAGE="${CI_DOCS_PACKAGE-files}"

if [ -n "${CI_METADATA_JSON:-}" ]; then
  meta="$(cat "$CI_METADATA_JSON")"
else
  meta="$(cargo metadata --format-version 1 --no-deps --locked)"
fi

root="$(jq -r '.workspace_root' <<<"$meta")"
declare -A dir_pkg=() pkg_dir=() rdeps=()
while IFS=$'\t' read -r dir name; do
  dir="${dir#"$root"/}"
  dir_pkg["$dir"]="$name"
  pkg_dir["$name"]="$dir"
done < <(jq -r '
  .workspace_members as $m
  | .packages[] | select(.id as $id | $m | index($id))
  | [(.manifest_path | rtrimstr("/Cargo.toml")), .name] | @tsv' <<<"$meta")

while IFS=$'\t' read -r dep dependent; do
  if [ -n "${pkg_dir[$dep]+x}" ]; then rdeps["$dep"]+="$dependent "; fi
done < <(jq -r '.packages[] as $p | $p.dependencies[] | select(.path != null) | [.name, $p.name] | @tsv' <<<"$meta")

emit_all() {
  echo "ci-affected-crates: whole workspace ($1)" >&2
  printf 'mode=all\nargs=--workspace\npackages=\nignore_regex=\n'
  exit 0
}

declare -A selected=()
seen=0
docs=0
while IFS= read -r path; do
  [ -n "$path" ] || continue
  seen=$((seen + 1))
  d="$path"
  pkg=""
  while :; do
    case "$d" in
      */*) d="${d%/*}" ;;
      *) break ;;
    esac
    if [ -n "${dir_pkg[$d]+x}" ]; then
      pkg="${dir_pkg[$d]}"
      break
    fi
  done
  if [ -n "$pkg" ]; then
    selected["$pkg"]=1
    continue
  fi
  case "$path" in
    docs/*) selected["$DOCS_PACKAGE"]=1 ;;
    *.md) docs=1 ;;
    LICENSE*) echo "ci-affected-crates: ignoring $path" >&2 ;;
    *) emit_all "$path maps to no crate" ;;
  esac
done

[ "$seen" -gt 0 ] || emit_all "no changed paths given"
if [ -n "${CI_EXPECTED_COUNT:-}" ] && [ "$seen" != "$CI_EXPECTED_COUNT" ]; then
  emit_all "received $seen paths, expected $CI_EXPECTED_COUNT"
fi

if [ "${#selected[@]}" -eq 0 ] && [ "$docs" = 0 ]; then
  echo "ci-affected-crates: no crate affected" >&2
  printf 'mode=none\nargs=\npackages=\nignore_regex=\n'
  exit 0
fi

queue=("${!selected[@]}")
while [ "${#queue[@]}" -gt 0 ]; do
  pkg="${queue[0]}"
  queue=("${queue[@]:1}")
  for dependent in ${rdeps[$pkg]-}; do
    if [ -z "${selected[$dependent]+x}" ]; then
      selected["$dependent"]=1
      queue+=("$dependent")
    fi
  done
done

# Added after the closure: they run for their repo-walking tests, which a
# change does not make their dependents need.
for pkg in $ALWAYS_PACKAGES; do
  if [ -n "${pkg_dir[$pkg]+x}" ]; then selected["$pkg"]=1; fi
done

if [ "${#selected[@]}" -eq "${#pkg_dir[@]}" ]; then
  emit_all "every member is affected"
fi

mapfile -t pkgs < <(printf '%s\n' "${!selected[@]}" | LC_ALL=C sort)
args=""
for pkg in "${pkgs[@]}"; do args+="-p $pkg "; done

untested=()
for pkg in "${!pkg_dir[@]}"; do
  [ -n "${selected[$pkg]+x}" ] || untested+=("$(printf '%s' "${pkg_dir[$pkg]}" | sed 's/[][\.*^$+?(){}|]/\\&/g')")
done
root_re="$(printf '%s' "$root" | sed 's/[][\.*^$+?(){}|]/\\&/g')"
ignore_regex="^${root_re}/($(printf '%s\n' "${untested[@]}" | LC_ALL=C sort | paste -sd'|' -))/"

echo "ci-affected-crates: testing ${pkgs[*]}" >&2
printf 'mode=partial\nargs=%s\npackages=%s\nignore_regex=%s\n' "${args% }" "${pkgs[*]}" "$ignore_regex"
