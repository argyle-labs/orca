#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stub_dir="$(mktemp -d)"
trap 'rm -rf "$stub_dir"' EXIT
log="$stub_dir/args"

cat > "$stub_dir/codesign" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$@" > "$log"
STUB
chmod +x "$stub_dir/codesign"

# shellcheck source=release-lib.sh
. "$root/scripts/release-lib.sh"

check() {
  local name="$1" expected="$2"
  local actual
  actual="$(tr '\n' ' ' < "$log")"
  if [ "$actual" != "$expected" ]; then
    echo "FAIL $name: got '$actual' want '$expected'" >&2
    exit 1
  fi
  echo "ok $name"
}

PATH="$stub_dir:$PATH" ORCA_CODESIGN_IDENTITY="" ORCA_CODESIGN_KEYCHAIN="" codesign_orca /bin/orca
check ad-hoc "--force --sign - --identifier wtf.argyle.orca --timestamp=none /bin/orca "

PATH="$stub_dir:$PATH" ORCA_CODESIGN_IDENTITY="Argyle Labs orca Code Signing" ORCA_CODESIGN_KEYCHAIN="/k/orca.keychain-db" codesign_orca /bin/orca
check keychain "--force --sign Argyle Labs orca Code Signing --identifier wtf.argyle.orca --timestamp=none --options runtime --keychain /k/orca.keychain-db /bin/orca "
