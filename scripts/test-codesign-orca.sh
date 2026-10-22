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

cat > "$stub_dir/security" <<STUB
#!/usr/bin/env bash
python3 -c 'import shlex, sys; sys.stdout.write("\n".join(shlex.split(sys.stdin.read())) + "\n")' > "$log"
STUB
chmod +x "$stub_dir/security"

# shellcheck disable=SC2016
for value in 'pa"ss' 'b\ack\\slash' 'with space' '100%s' '$HOME $(id)' 'mix "\ $x %d\"'; do
  printf 'unlock-keychain -p %s %s\n' "$(secq "$value")" "$(secq "/k/$value")" | PATH="$stub_dir:$PATH" security -i
  expected="$(printf 'unlock-keychain\n-p\n%s\n/k/%s' "$value" "$value")"
  actual="$(cat "$log")"
  if [ "$actual" != "$expected" ]; then
    echo "FAIL secq '$value': got '$actual'" >&2
    exit 1
  fi
done
echo "ok secq round-trip"

if secq $'a\nb' 2>/dev/null; then
  echo "FAIL secq accepted a newline" >&2
  exit 1
fi
echo "ok secq rejects newline"
