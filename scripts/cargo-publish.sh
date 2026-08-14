#!/usr/bin/env bash
# Publish the plugin-toolkit dependency closure to the `orca` Cargo registry,
# leaves-first. The registry endpoint and token are supplied ENTIRELY via the
# environment — this script hardcodes NO address (gitea has many reachable
# addresses; the caller/orca resolves a reachable one). Required env:
#
#   CARGO_REGISTRIES_ORCA_INDEX  sparse+<resolved-gitea>/api/packages/<owner>/cargo/
#   CARGO_REGISTRIES_ORCA_TOKEN  Gitea token with package:write scope
#
# Publish order is a leaves-first topological sort of the closure DAG; each
# crate must exist on the registry before its dependents publish. We poll the
# sparse index for the just-published crate before moving on, because Gitea
# makes a new version queryable a beat after the upload returns.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

: "${CARGO_REGISTRIES_ORCA_INDEX:?set CARGO_REGISTRIES_ORCA_INDEX (sparse+<url>/api/packages/<owner>/cargo/)}"
: "${CARGO_REGISTRIES_ORCA_TOKEN:?set CARGO_REGISTRIES_ORCA_TOKEN (gitea package:write token)}"

# Gitea's Cargo registry is an auth-required sparse registry: cargo needs a
# credential provider, and the token must carry an auth SCHEME. Gitea expects
# `Bearer <token>`. Accept a raw token from the caller and normalize it, keeping
# the un-schemed value for the index HTTP poll (which uses the `token` scheme).
export CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS="cargo:token"
RAW_TOKEN="${CARGO_REGISTRIES_ORCA_TOKEN#Bearer }"
export CARGO_REGISTRIES_ORCA_TOKEN="Bearer ${RAW_TOKEN}"

# Leaves-first; plugin-toolkit last. Matches the closure DAG (no cycles).
ORDER=(
  plugin-abi plugin-proto derive utils contract dispatch macro-runtime
  notifications containers deploy-target storage service db graphql openapi
  plugin-toolkit
)

# Base HTTP url + owner for the poll, derived from the sparse index url.
INDEX_HTTP="${CARGO_REGISTRIES_ORCA_INDEX#sparse+}"   # strip cargo's sparse+ prefix

wait_for_crate() {
  local name="$1" version="$2"
  # Sparse index path: lowercased name is sharded 1/2/3+ chars (cargo convention).
  local n="${name//_/-}"; n="${n,,}"
  local path
  case ${#n} in
    1) path="1/${n}" ;;
    2) path="2/${n}" ;;
    3) path="3/${n:0:1}/${n}" ;;
    *) path="${n:0:2}/${n:2:2}/${n}" ;;
  esac
  for _ in $(seq 1 30); do
    if curl -sf -H "Authorization: token ${RAW_TOKEN}" \
         "${INDEX_HTTP%/}/${path}" 2>/dev/null | grep -q "\"vers\":\"${version}\""; then
      return 0
    fi
    sleep 2
  done
  echo "::warning::index did not surface ${name} ${version} after 60s; continuing" >&2
}

# Inject transient publish versions (reverted by the caller / git after run).
"${REPO_ROOT}/scripts/cargo-publish-prep.sh"

VERSION="$(grep -m1 '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')"
ABI_VERSION="$(grep -m1 '^version = ' projects/plugin-abi/Cargo.toml | sed 's/version = "\(.*\)"/\1/')"

for crate in "${ORDER[@]}"; do
  ver="$VERSION"; [ "$crate" = "plugin-abi" ] && ver="$ABI_VERSION"
  echo "── publishing ${crate} ${ver} → orca registry"
  if cargo publish -p "$crate" --registry orca --no-verify --allow-dirty 2>&1 | tee /tmp/pub.$crate.log \
     | grep -qiE 'already (exists|uploaded)'; then
    echo "   ${crate} ${ver} already on registry — skipping"
    continue
  fi
  wait_for_crate "$crate" "$ver"
done

echo "✓ published closure (${#ORDER[@]} crates) at ${VERSION}"
