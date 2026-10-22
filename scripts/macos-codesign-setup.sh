#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: ORCA_CODESIGN_KEYCHAIN_PASSWORD=... ORCA_CODESIGN_P12_PASSWORD=... $0 <identity.p12>" >&2
  exit 2
}

[ "$(uname -s)" = "Darwin" ] || { echo "error: macOS only" >&2; exit 1; }
[ $# -eq 1 ] && [ -f "$1" ] || usage
: "${ORCA_CODESIGN_KEYCHAIN_PASSWORD:?set ORCA_CODESIGN_KEYCHAIN_PASSWORD}"
: "${ORCA_CODESIGN_P12_PASSWORD:?set ORCA_CODESIGN_P12_PASSWORD}"

p12="$1"
kc="$HOME/Library/Keychains/orca-codesign.keychain-db"
identity="Argyle Labs orca Code Signing"

sec() {
  local cmd="$1"
  printf '%s\n' "$cmd" | security -i
}

if [ ! -f "$kc" ]; then
  sec "$(printf 'create-keychain -p "%s" "%s"' "$ORCA_CODESIGN_KEYCHAIN_PASSWORD" "$kc")"
fi
sec "$(printf 'unlock-keychain -p "%s" "%s"' "$ORCA_CODESIGN_KEYCHAIN_PASSWORD" "$kc")"
security set-keychain-settings -lut 3600 "$kc"
sec "$(printf 'import "%s" -k "%s" -f pkcs12 -P "%s" -T /usr/bin/codesign' "$p12" "$kc" "$ORCA_CODESIGN_P12_PASSWORD")"
sec "$(printf 'set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "%s" "%s"' "$ORCA_CODESIGN_KEYCHAIN_PASSWORD" "$kc")" >/dev/null

existing=$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')
if ! printf '%s\n' "$existing" | grep -qxF "$kc"; then
  # shellcheck disable=SC2086
  security list-keychains -d user -s "$kc" $existing
fi

security find-identity -v -p codesigning "$kc" | grep -qF "\"$identity\"" \
  || { echo "error: '$identity' not found in $kc after import" >&2; exit 1; }
security lock-keychain "$kc"
echo "provisioned '$identity' in $kc"
