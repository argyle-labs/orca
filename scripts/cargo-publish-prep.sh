#!/usr/bin/env bash
# Inject publish-required `version` fields into the internal path dependencies
# of the plugin-toolkit publish closure, transiently, right before
# `cargo publish`. cargo rejects a `path` dep that carries no `version`; we do
# NOT commit these versions (they would go stale on every release bump — the
# bump only rewrites the single `[workspace.package] version` line). Instead we
# derive them here from that one source of truth at publish time.
#
# Rule: deps on `plugin-abi` pin its FROZEN version (see its Cargo.toml — the
# ABI is a deliberate flag-day, NOT workspace-versioned); every other internal
# dep gets the workspace version. `path` is retained so a same-tree build still
# resolves locally; cargo drops `path` and keeps `version` in the published
# manifest.
#
# Idempotent: re-running is a no-op on already-injected lines.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECTS="${REPO_ROOT}/projects"

# The workspace version = single source of truth (bumped by write_cargo_version).
WS_VERSION="$(grep -m1 '^version = ' "${REPO_ROOT}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"
# The frozen ABI version, read from the crate itself — never assumed.
ABI_VERSION="$(grep -m1 '^version = ' "${PROJECTS}/plugin-abi/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"

[ -n "$WS_VERSION" ]  || { echo "could not read workspace version" >&2; exit 1; }
[ -n "$ABI_VERSION" ] || { echo "could not read plugin-abi version" >&2; exit 1; }
echo "workspace version=${WS_VERSION}  plugin-abi(frozen)=${ABI_VERSION}"

# crate -> internal path deps to inject (derived from the toolkit dep closure).
prep_file() {
  local crate="$1"; shift
  local f="${PROJECTS}/${crate}/Cargo.toml"
  python3 - "$f" "$WS_VERSION" "$ABI_VERSION" "$@" <<'PY'
import re, sys
f, ws, abi, *deps = sys.argv[1:]
want = set(deps)
out = []
for line in open(f):
    s = line.lstrip()
    if re.search(r'path\s*=\s*"\.\./', s) and not s.startswith('#') and '{' in line:
        # crate name: `package = "X"` wins, else the table key
        m = re.search(r'package\s*=\s*"([^"]+)"', line)
        name = m.group(1) if m else line.split('=', 1)[0].strip()
        if name in want and 'version' not in line.split('{', 1)[1]:
            ver = f'version = "{abi}"' if name == 'plugin-abi' else f'version = "{ws}"'
            # `registry = "orca"` is REQUIRED so cargo resolves the dep from the
            # orca registry at publish time; without it cargo defaults to
            # crates.io and fails (finds an unrelated crate of the same name).
            # `path` is kept so local/same-tree builds still resolve from disk.
            head, _, tail = line.partition('{')
            line = f'{head}{{ {ver}, registry = "orca", {tail.lstrip()}'
    out.append(line)
open(f, 'w').write(''.join(out))
PY
}

prep_file contract       utils
prep_file dispatch       contract
prep_file macro-runtime  contract derive dispatch utils
prep_file notifications  macro-runtime derive
prep_file containers     macro-runtime utils contract derive notifications
prep_file storage        derive
prep_file service        deploy-target contract
prep_file db             macro-runtime plugin-abi utils contract derive
prep_file graphql        utils
prep_file openapi        utils
prep_file plugin-toolkit plugin-abi plugin-proto derive storage service \
                         deploy-target contract db dispatch macro-runtime \
                         notifications containers utils graphql openapi

echo "injected publish versions into ${PROJECTS} closure (transient — do not commit)"
