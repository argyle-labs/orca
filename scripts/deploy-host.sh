#!/usr/bin/env bash
# Controller-side push deploy. Use when the target host can't reach GitHub
# (private network, VPN-restricted, no curl/wget). Runs ON YOUR LAPTOP, uses
# ssh + scp to ship binary + install.sh, then invokes install.sh on the host.
#
# Pull mode (the one-liner in docs/install-runbook.md) remains the primary
# path. This script is the fallback for hosts where pull mode fails.
#
# Usage:
#   scripts/deploy-host.sh [--user <user>] [--version <tag>] [--prerelease] <host>
#
# Defaults:
#   user         root  (install.sh will pivot to the `orca` service user)
#   version      latest -rc.* tag from GitHub
#   prerelease   on (this script is primarily used for RC fleet deploys)
#
# Requirements on controller: gh, scp, ssh, sha256sum (or shasum).
# Requirements on target: sh, mv, chmod, mkdir, sha256sum (or shasum).
# The target does NOT need curl, wget, or outbound GitHub access.

set -euo pipefail

USER_OVERRIDE=""
VERSION=""
PRERELEASE=1
HOST=""

while [ $# -gt 0 ]; do
  case "$1" in
    --user)        USER_OVERRIDE="$2"; shift 2 ;;
    --version)     VERSION="$2"; shift 2 ;;
    --prerelease)  PRERELEASE=1; shift ;;
    --stable)      PRERELEASE=0; shift ;;
    -h|--help)
      sed -n '2,20p' "$0"; exit 0 ;;
    -*)
      echo "unknown flag: $1" >&2; exit 2 ;;
    *)
      [ -z "$HOST" ] || { echo "extra positional arg: $1" >&2; exit 2; }
      HOST="$1"; shift ;;
  esac
done

[ -n "$HOST" ] || { echo "usage: deploy-host.sh [--user <u>] [--version <v>] <host>" >&2; exit 2; }
REMOTE_USER="${USER_OVERRIDE:-root}"

die() { echo "deploy-host: $*" >&2; exit 1; }

command -v gh >/dev/null || die "gh not installed on controller"
command -v scp >/dev/null || die "scp not installed on controller"
command -v ssh >/dev/null || die "ssh not installed on controller"

# Repo + tag resolution
REPO="argyle-labs/orca"
if [ -z "$VERSION" ]; then
  if [ "$PRERELEASE" = "1" ]; then
    VERSION=$(gh release list --repo "$REPO" --limit 30 --json tagName,isPrerelease \
      --jq 'map(select(.isPrerelease)) | .[0].tagName')
  else
    VERSION=$(gh release view --repo "$REPO" --json tagName --jq .tagName)
  fi
fi
[ -n "$VERSION" ] || die "could not resolve version"

# Probe target's libc + arch over SSH so we pick the right asset.
echo "→ probing $REMOTE_USER@$HOST"
PROBE=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$REMOTE_USER@$HOST" \
  'echo "$(uname -s)|$(uname -m)|$(ldd --version 2>&1 | head -1)"' )
OS=$(echo "$PROBE"  | awk -F'|' '{print $1}')
ARCH=$(echo "$PROBE" | awk -F'|' '{print $2}')
LDD=$(echo "$PROBE"  | awk -F'|' '{print $3}')

case "$ARCH" in
  aarch64|arm64) ARCH=aarch64 ;;
  x86_64|amd64)  ARCH=x86_64  ;;
  *) die "unsupported arch: $ARCH" ;;
esac

case "$OS" in
  Linux)
    LIBC=gnu
    echo "$LDD" | grep -qi musl && LIBC=musl
    TARGET="${ARCH}-unknown-linux-${LIBC}"
    ;;
  Darwin)
    TARGET="${ARCH}-apple-darwin" ;;
  *)
    die "unsupported os: $OS" ;;
esac

STAGING="${TMPDIR:-/tmp}/orca-deploy-${VERSION}"
mkdir -p "$STAGING"
_asset_ver="${VERSION#v}"
ASSET="orca-${_asset_ver}-${TARGET}"
ASSET_SHA="${ASSET}.sha256"

# Fetch once on the controller (cached by version).
if [ ! -f "$STAGING/$ASSET" ] || [ ! -f "$STAGING/$ASSET_SHA" ]; then
  echo "→ fetching $VERSION / $ASSET"
  gh release download "$VERSION" --repo "$REPO" --pattern "$ASSET" --pattern "$ASSET_SHA" --dir "$STAGING" --clobber
fi

# Pubkey to install for the orca user. First key wins.
PUBKEY=""
for k in ~/.ssh/id_ed25519.pub ~/.ssh/id_ecdsa.pub ~/.ssh/id_rsa.pub; do
  [ -f "$k" ] && PUBKEY="$(cat "$k")" && break
done
[ -n "$PUBKEY" ] || die "no SSH pubkey found in ~/.ssh — run ssh-keygen -t ed25519 first"

# Resolve install.sh next to this script.
INSTALL_SH="$(dirname "$(realpath "$0")")/install.sh"
[ -f "$INSTALL_SH" ] || die "install.sh not found at $INSTALL_SH"

echo "→ uploading bytes to $REMOTE_USER@$HOST:/tmp/"
scp -q "$STAGING/$ASSET" "$STAGING/$ASSET_SHA" "$INSTALL_SH" "$REMOTE_USER@$HOST:/tmp/"

REMOTE_BIN="/tmp/$ASSET"
REMOTE_SHA="/tmp/$ASSET_SHA"
REMOTE_INSTALLER="/tmp/install.sh"

# Normalize the sha file name so install.sh finds `${FROM_FILE}.sha256` next to it.
# Then run installer in --from-file mode with the admin pubkey. Tokenless.
echo "→ installing on $REMOTE_USER@$HOST"
ssh "$REMOTE_USER@$HOST" "
set -eu
mv '$REMOTE_BIN' /tmp/orca
mv '$REMOTE_SHA' /tmp/orca.sha256
chmod +x '$REMOTE_INSTALLER'
'$REMOTE_INSTALLER' \
  --from-file /tmp/orca \
  --version '$VERSION' \
  --target '$TARGET' \
  --prerelease \
  --admin-pubkey '$PUBKEY'
rm -f /tmp/orca /tmp/orca.sha256 '$REMOTE_INSTALLER'
"

echo "✓ $HOST deployed ($VERSION, $TARGET)"
