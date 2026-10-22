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

# shellcheck source=release-lib.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/release-lib.sh"

q_kc_pw=$(secq "$ORCA_CODESIGN_KEYCHAIN_PASSWORD")
q_p12_pw=$(secq "$ORCA_CODESIGN_P12_PASSWORD")
q_kc=$(secq "$kc")
q_p12=$(secq "$p12")

sec() {
  printf '%s\n' "$1" | security -i
}

if [ ! -f "$kc" ]; then
  sec "create-keychain -p $q_kc_pw $q_kc"
  security show-keychain-info "$kc" >/dev/null 2>&1 \
    || { echo "error: failed to create $kc" >&2; exit 1; }
fi
sec "unlock-keychain -p $q_kc_pw $q_kc"
security show-keychain-info "$kc" >/dev/null 2>&1 \
  || { echo "error: unlock failed for $kc" >&2; exit 1; }
security set-keychain-settings -lut 3600 "$kc"
sec "import $q_p12 -k $q_kc -f pkcs12 -P $q_p12_pw -T /usr/bin/codesign"
security find-identity -v -p codesigning "$kc" | grep -qF "\"$identity\"" \
  || { echo "error: '$identity' not found in $kc after import" >&2; exit 1; }
sec "set-key-partition-list -S apple-tool:,apple:,codesign: -s -k $q_kc_pw $q_kc" >/dev/null

existing=()
while IFS= read -r line; do
  existing+=("$line")
done < <(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')
found=0
for k in ${existing[@]+"${existing[@]}"}; do
  [ "$k" = "$kc" ] && found=1
done
if [ "$found" -eq 0 ]; then
  security list-keychains -d user -s "$kc" ${existing[@]+"${existing[@]}"}
fi

security find-identity -v -p codesigning "$kc" | grep -qF "\"$identity\"" \
  || { echo "error: '$identity' not found in $kc after import" >&2; exit 1; }
security lock-keychain "$kc"
echo "provisioned '$identity' in $kc"
