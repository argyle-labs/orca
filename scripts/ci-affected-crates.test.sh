#!/usr/bin/env bash
# Tests scripts/ci-affected-crates.sh against a small fake cargo workspace.
# Every *-on-core crate reaches core through a different kind of path dep;
# app reaches it transitively through mid; leaf stands alone.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/ci-affected-crates.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
WORK="$(pwd -P)" # cargo reports the canonical path

crate() {
  mkdir -p "crates/$1/src"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n%s\n' "$1" "$2" >"crates/$1/Cargo.toml"
  : >"crates/$1/src/lib.rs"
}
printf '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n\n[workspace.dependencies]\ncore = { path = "crates/core" }\n' >Cargo.toml
crate core ''
crate mid $'[dependencies]\ncore = { path = "../core" }'
crate app $'[dependencies]\nmid = { path = "../mid" }'
crate dev-on-core $'[dev-dependencies]\ncore = { path = "../core" }'
crate build-on-core $'[build-dependencies]\ncore = { path = "../core" }'
crate target-on-core $'[target.\'cfg(unix)\'.dependencies]\ncore = { path = "../core" }'
crate opt-on-core $'[dependencies]\ncore = { path = "../core", optional = true }'
crate renamed-on-core $'[dependencies]\nkernel = { package = "core", path = "../core" }'
crate ws-on-core $'[dependencies]\ncore = { workspace = true }'
crate leaf ''
crate walker ''
cargo metadata --format-version 1 --no-deps --offline >meta.json

fail=0
run() { printf '%s\n' "$@" | CI_METADATA_JSON=meta.json "$SCRIPT" 2>/dev/null; }
expect() {
  local name="$1" want="$2" got
  shift 2
  got="$(run "$@" | grep -E '^(mode|packages)=' | paste -sd' ' -)"
  if [ "$got" != "$want" ]; then
    echo "FAIL: $name: got '$got', want '$want'"
    fail=1
  fi
}

export CI_ALWAYS_PACKAGES="" CI_DOCS_PACKAGE=leaf
expect "changed leaf" "mode=partial packages=leaf" crates/leaf/src/lib.rs
expect "changed lib pulls in every kind of dependent" \
  "mode=partial packages=app build-on-core core dev-on-core mid opt-on-core renamed-on-core target-on-core ws-on-core" \
  crates/core/src/lib.rs
expect "mid change skips its dependency" "mode=partial packages=app mid" crates/mid/Cargo.toml
expect "unmappable root file" "mode=all packages=" crates/leaf/src/lib.rs Cargo.lock
expect "workflow file" "mode=all packages=" .github/workflows/ci.yml
expect "license only" "mode=none packages=" LICENSE
expect "docs/ maps to the docs package" "mode=partial packages=leaf" docs/guide.md
expect "every member affected" "mode=all packages=" crates/core/src/lib.rs crates/leaf/src/lib.rs crates/walker/src/lib.rs

export CI_ALWAYS_PACKAGES=walker
expect "always packages join a partial run" "mode=partial packages=leaf walker" crates/leaf/src/lib.rs
expect "root markdown selects only the always packages" "mode=partial packages=walker" README.md
expect "license only stays none" "mode=none packages=" LICENSE
export CI_ALWAYS_PACKAGES=core
expect "always packages do not pull in their dependents" "mode=partial packages=core leaf" crates/leaf/src/lib.rs

got="$(printf 'crates/leaf/src/lib.rs\n' | CI_EXPECTED_COUNT=2 CI_METADATA_JSON=meta.json "$SCRIPT" 2>/dev/null | sed -n 's/^mode=//p')"
[ "$got" = all ] || { echo "FAIL: path count mismatch gave mode=$got, want all"; fail=1; }

export CI_ALWAYS_PACKAGES=""
re="$(run crates/leaf/src/lib.rs | sed -n 's/^ignore_regex=//p')"
grep -Eq "$re" <<<"$WORK/crates/core/src/lib.rs" || { echo "FAIL: ignore_regex misses untested core"; fail=1; }
! grep -Eq "$re" <<<"$WORK/crates/leaf/src/lib.rs" || { echo "FAIL: ignore_regex hides tested leaf"; fail=1; }

[ "$fail" = 0 ] && echo "ci-affected-crates: all checks passed"
exit "$fail"
