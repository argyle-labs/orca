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
IFS= read -r line
out=() tok="" have=0 inq=0 i=0
while [ "\$i" -lt "\${#line}" ]; do
  c="\${line:\$i:1}"
  if [ "\$inq" -eq 1 ]; then
    if [ "\$c" = "\\\\" ]; then
      i=\$((i + 1))
      tok+="\${line:\$i:1}"
    elif [ "\$c" = '"' ]; then
      inq=0
    else
      tok+="\$c"
    fi
  elif [ "\$c" = '"' ]; then
    inq=1 have=1
  elif [ "\$c" = " " ]; then
    [ "\$have" -eq 1 ] && out+=("\$tok")
    tok="" have=0
  else
    tok+="\$c" have=1
  fi
  i=\$((i + 1))
done
[ "\$have" -eq 1 ] && out+=("\$tok")
printf '%s\\n' "\${out[@]}" > "$log"
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

if [ "$(uname -s)" = "Darwin" ]; then
  missing="$stub_dir/no such \"dir\"/k.keychain-db"
  if err="$(printf 'show-keychain-info %s\n' "$(secq "$missing")" | /usr/bin/security -i 2>&1)"; then
    echo "FAIL real security accepted nonexistent keychain" >&2
    exit 1
  fi
  case "$err" in
    *"$missing"*) echo "ok real security parses quoted path" ;;
    *) echo "FAIL real security error lacks '$missing': $err" >&2; exit 1 ;;
  esac
else
  echo "skip real security (non-Darwin)"
fi
