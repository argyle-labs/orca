#!/usr/bin/env bash
# Tests scripts/ci-target-cache.sh against a scratch git repo. Linux (GNU
# coreutils), like the CI job it serves.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/ci-target-cache.sh"
unset CARGO_TARGET_DIR
if ! stat -c %Y / >/dev/null 2>&1 || ! touch -c -d @0 /nonexistent-ci-target-cache 2>/dev/null; then
  echo "ci-target-cache test: skipped (needs GNU stat/touch)"
  exit 0
fi
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

fail=0
old() { [ "$(stat -c %Y -- "$1")" -lt 1000000000 ]; }
expect_old() { old "$1" || { echo "FAIL: $1 should be backdated"; fail=1; }; }
expect_now() { ! old "$1" || { echo "FAIL: $1 should keep checkout mtime"; fail=1; }; }

git init -q .
git config user.email ci@example.invalid
git config user.name ci
mkdir -p crates/db/migrations crates/db/src crates/app/src "crates/app/src/with space" \
  crates/gone/src scripts
echo '[package]' >crates/db/Cargo.toml
echo '[package]' >crates/app/Cargo.toml
echo '[workspace]' >Cargo.toml
printf 'target/\n*.json\n' >.gitignore
echo 'select 1;' >crates/db/migrations/0001.sql
echo 'select 2;' >crates/db/migrations/0002.sql
echo 'fn a() {}' >crates/db/src/lib.rs
echo 'fn b() {}' >crates/app/src/lib.rs
echo 'fn c() {}' >crates/app/src/main.rs
echo 'x' >"crates/app/src/with space/f.rs"
echo '[package]' >crates/gone/Cargo.toml
echo 'fn g() {}' >crates/gone/src/lib.rs
echo 'echo hi' >scripts/a.sh
git add -A && git commit -qm base

mkdir -p target/debug/deps
"$SCRIPT" record
echo stale >target/x.profraw

# Next run: a new migration in db, one edited file in app.
echo 'select 3;' >crates/db/migrations/0003.sql
echo 'fn c() { 1; }' >crates/app/src/main.rs
git add -A
touch crates/db/migrations/* crates/db/src/lib.rs crates/db/Cargo.toml crates/app/src/* "crates/app/src/with space/f.rs" Cargo.toml

"$SCRIPT" restore

expect_now crates/db/migrations/0001.sql
expect_now crates/db/src/lib.rs
expect_now crates/db/Cargo.toml
expect_now crates/db/migrations/0003.sql
expect_now crates/app/src/main.rs
expect_old crates/app/src/lib.rs
expect_old "crates/app/src/with space/f.rs"
expect_old crates/app/Cargo.toml
expect_old Cargo.toml
expect_old .git/HEAD
expect_old .git/index
[ ! -e target/x.profraw ] || { echo "FAIL: profraw survived restore"; fail=1; }
[ ! -e target/.ci-cache-complete ] || { echo "FAIL: sentinel survived restore"; fail=1; }

# Removal of a tracked path also dirties its crate.
"$SCRIPT" record
git rm -q crates/app/src/lib.rs
touch crates/app/src/main.rs crates/db/src/lib.rs
"$SCRIPT" restore
expect_now crates/app/src/main.rs
expect_old crates/db/src/lib.rs

# A file added outside any crate (root is a virtual workspace) dirties nothing.
"$SCRIPT" record
echo 'echo new' >scripts/b.sh
git add -A
touch crates/db/src/lib.rs crates/app/src/main.rs scripts/a.sh
"$SCRIPT" restore
expect_old crates/db/src/lib.rs
expect_old crates/app/src/main.rs
expect_old scripts/a.sh
expect_now scripts/b.sh

# Mode change is a change to that file only.
"$SCRIPT" record
chmod +x scripts/a.sh
git add -A
touch scripts/a.sh crates/db/src/lib.rs
"$SCRIPT" restore
expect_now scripts/a.sh
expect_old crates/db/src/lib.rs

# A whole crate removed leaves the other crates cached.
"$SCRIPT" record
git rm -rq crates/gone
touch crates/db/src/lib.rs crates/app/src/main.rs
"$SCRIPT" restore
expect_old crates/db/src/lib.rs
expect_old crates/app/src/main.rs

# A root [package] makes a root-level addition dirty everything.
"$SCRIPT" record
printf '[package]\n' >>Cargo.toml
git add -A
"$SCRIPT" record
echo 'notes' >NOTES.md
git add -A
touch crates/db/src/lib.rs
"$SCRIPT" restore
expect_now crates/db/src/lib.rs

# No sentinel (incomplete cache): nothing is backdated.
"$SCRIPT" record
rm target/.ci-cache-complete
touch crates/db/src/lib.rs
"$SCRIPT" restore
expect_now crates/db/src/lib.rs

# prune keeps executables the build listed (and their hardlinks), drops the rest.
printf '#!/bin/sh\n' >target/debug/deps/live-abc && chmod +x target/debug/deps/live-abc
ln target/debug/deps/live-abc target/debug/live
cp target/debug/deps/live-abc target/debug/deps/stale-def
cp target/debug/deps/live-abc target/debug/deps/libmacro-1.so
printf '{"reason":"compiler-artifact","executable":"%s/target/debug/live"}\n' "$PWD" >msgs.json
"$SCRIPT" prune msgs.json
[ -e target/debug/deps/live-abc ] || { echo "FAIL: live executable pruned"; fail=1; }
[ ! -e target/debug/deps/stale-def ] || { echo "FAIL: stale executable kept"; fail=1; }
[ -e target/debug/deps/libmacro-1.so ] || { echo "FAIL: non-test artifact pruned"; fail=1; }

# An empty message file must never prune everything.
: >empty.json
rc=0
"$SCRIPT" prune empty.json || rc=$?
[ "$rc" = 3 ] || { echo "FAIL: prune with no listed executables returned $rc, want 3"; fail=1; }
[ -e target/debug/deps/live-abc ] || { echo "FAIL: empty message file pruned executables"; fail=1; }

[ "$fail" = 0 ] && echo "ci-target-cache: all checks passed"
exit "$fail"
