#!/usr/bin/env bash
# Publishes every publishable workspace crate to the `gitea` registry (defined
# in .cargo/config.toml) in dependency order (normal + build edges; dev deps
# are stripped on publish). Exists instead of `cargo publish --workspace`
# because that aborts on a version already in the registry; here an existing
# version is skipped, so a rerun after a partial failure resumes where it
# stopped.
#
# Requires CARGO_REGISTRIES_GITEA_TOKEN ("Bearer <token>"). `--dry-run` prints
# the order and each crate's index state without publishing.
set -euo pipefail

cd "$(dirname "$0")/.."
dry_run=false
[ "${1:-}" = "--dry-run" ] && dry_run=true

index="${CARGO_REGISTRIES_GITEA_INDEX:-$(sed -n '/^\[registries.gitea\]/,/^\[/s/^index = "\(.*\)"$/\1/p' .cargo/config.toml)}"
index="${index#sparse+}"
[ -n "$index" ] || { echo "no [registries.gitea] index configured" >&2; exit 1; }

meta="$(cargo metadata --format-version 1 --no-deps --locked)"

# name<TAB>version, publishable members only, dependencies before dependents.
order="$(jq -r '
  .workspace_members as $m
  | [.packages[] | select(.id as $id | $m | index($id))
     ] as $all
  | ($all | map(.name)) as $members
  | [$all[] | select(.publish | index("gitea"))] as $ws
  | ($ws | map(.name)) as $names
  | ($ws | map({key: .name, value: [.dependencies[]
       | select(.path != null and .kind != "dev")
       | .name | select(. as $d | $members | index($d))] | unique}) | from_entries) as $g
  | ([$g | to_entries[] | .key as $k | .value[]
       | select(. as $d | $names | index($d) | not) | "\($k) -> \(.)"]) as $bad
  | if ($bad | length) > 0
    then error("publishable crates depend on unpublishable members: \($bad | join(", "))")
    else . end
  | ($ws | map({key: .name, value: .version}) | from_entries) as $ver
  | def drain($done):
      ([$g | to_entries[] | select((.key as $k | $done | index($k)) | not)
            | select(all(.value[]; . as $d | $done | index($d))) | .key] | sort) as $ready
      | if ($ready | length) == 0 then $done else drain($done + $ready) end;
    drain([]) as $placed
  | if ($placed | length) != ($g | length)
    then error("dependency cycle among: \($g | keys - $placed | join(" "))")
    else $placed[] | [., $ver[.]] | @tsv end' <<<"$meta")"

# Sparse index path: 1/, 2/, 3/<c>/, else <ab>/<cd>/ (lowercased).
index_path() {
  local n
  n="$(tr '[:upper:]' '[:lower:]' <<<"$1")"
  case ${#n} in
    1) echo "1/$n" ;;
    2) echo "2/$n" ;;
    3) echo "3/${n:0:1}/$n" ;;
    *) echo "${n:0:2}/${n:2:2}/$n" ;;
  esac
}

published() {
  local name="$1" version="$2" body code
  body="$(mktemp)"
  trap 'rm -f "$body"; trap - RETURN' RETURN
  # Header on stdin keeps the token out of the process list.
  code="$(printf '%s\n' "${CARGO_REGISTRIES_GITEA_TOKEN:+Authorization: $CARGO_REGISTRIES_GITEA_TOKEN}" \
    | curl -sS -o "$body" -w '%{http_code}' -H @- \
    "${index%/}/$(index_path "$name")")"
  case "$code" in
    200) jq -e --arg v "$version" 'select(.vers == $v)' "$body" >/dev/null && return 0 ;;
    404) ;;
    *) echo "index lookup for $name returned HTTP $code" >&2; exit 1 ;;
  esac
  return 1
}

while IFS=$'\t' read -r name version; do
  if published "$name" "$version"; then
    echo "skip    $name $version (already in registry)"
  elif $dry_run; then
    echo "would   $name $version"
  else
    echo "publish $name $version"
    # CI built and tested this commit; skip cargo's rebuild of each package.
    cargo publish --locked --no-verify --registry gitea -p "$name"
    # Dependents resolve against the index; wait until this version is visible.
    for attempt in $(seq 1 30); do
      published "$name" "$version" && break
      [ "$attempt" -eq 30 ] && { echo "$name $version not in index after publish" >&2; exit 1; }
      sleep 10
    done
  fi
done <<<"$order"
