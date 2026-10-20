#!/usr/bin/env bash
# Shipped builds must never enable utils' `test-scope` feature: it exposes
# keyless constructors for core-verb and daemon invocation scopes.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
found=0
for pkg in orca-server orca-plugin-toolkit; do
  if cargo tree --locked -p "$pkg" -e normal,build,features --prefix none | grep -q 'feature "test-scope"'; then
    echo "check-no-test-scope: $pkg enables orca-utils/test-scope in a non-dev build" >&2
    found=1
  fi
done
[ "$found" -eq 0 ] && echo "check-no-test-scope: clean"
exit "$found"
