#!/usr/bin/env sh
# orca installer — detects platform, downloads matching release binary,
# verifies sha256, installs to ~/.local/bin (or $ORCA_INSTALL_DIR).
#
# Usage:
#   curl -fsSL https://github.com/scottdkey/orca/releases/latest/download/install.sh | sh
#
# Flags / env overrides:
#   --version <tag>     ORCA_VERSION       e.g. v0.0.3-rc.4 (default: latest stable)
#   --target  <triple>  ORCA_TARGET        e.g. x86_64-unknown-linux-musl (default: auto-detect)
#   --dir     <path>    ORCA_INSTALL_DIR   install directory (default: ~/.local/bin)
#   --prerelease        ORCA_PRERELEASE=1  install newest prerelease (RC); pins channel=prerelease
#
# Channel marker is written to $ORCA_HOME/channel ($ORCA_HOME defaults to ~/.orca).
# The running app reads this to know which channel to pull future updates from.
#
# Examples:
#   sh install.sh
#   sh install.sh --version v0.0.3-rc.4
#   sh install.sh --target x86_64-unknown-linux-musl

set -eu

REPO="scottdkey/orca"
VERSION="${ORCA_VERSION:-}"
TARGET="${ORCA_TARGET:-}"
INSTALL_DIR="${ORCA_INSTALL_DIR:-$HOME/.local/bin}"
PRERELEASE="${ORCA_PRERELEASE:-0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version)     VERSION="$2"; shift 2 ;;
    --target)      TARGET="$2"; shift 2 ;;
    --dir)         INSTALL_DIR="$2"; shift 2 ;;
    --prerelease)  PRERELEASE=1; shift ;;
    -h|--help)     sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

die() { echo "install.sh: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

need curl
need chmod
need mv
need mkdir

# ── detect target triple ────────────────────────────────────────────────────
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$arch" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64)  arch=x86_64  ;;
    *) die "unsupported CPU architecture: $arch" ;;
  esac

  case "$os" in
    Darwin)
      echo "${arch}-apple-darwin"
      ;;
    Linux)
      # musl detection: alpine + anything where ldd reports musl, or no glibc present.
      libc=gnu
      if [ -f /etc/alpine-release ]; then
        libc=musl
      elif command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        libc=musl
      elif ! ls /lib*/libc.so.6 /lib/*/libc.so.6 >/dev/null 2>&1; then
        libc=musl
      fi
      echo "${arch}-unknown-linux-${libc}"
      ;;
    *) die "unsupported OS: $os (try --target)" ;;
  esac
}

if [ -z "$TARGET" ]; then
  TARGET="$(detect_target)"
fi

# ── resolve version ─────────────────────────────────────────────────────────
CHANNEL=stable
[ "$PRERELEASE" = "1" ] && CHANNEL=prerelease

if [ -z "$VERSION" ]; then
  if [ "$CHANNEL" = "prerelease" ]; then
    # Newest prerelease — /releases is ordered newest-first; pick the first
    # tag_name containing "-rc." (stable tags never do).
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=30" \
      | grep '"tag_name":' \
      | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' \
      | grep -m1 -- '-rc\.')"
    [ -n "$VERSION" ] || die "no prerelease found for ${REPO}"
  else
    # Latest stable — GitHub /releases/latest redirects to the stable tag.
    VERSION="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
      "https://github.com/${REPO}/releases/latest" \
      | sed -E 's#.*/tag/##')"
    [ -n "$VERSION" ] || die "could not resolve latest stable (try --version or --prerelease)"
  fi
else
  # Explicit --version: infer channel from tag shape so the marker reflects
  # what the user actually installed.
  case "$VERSION" in
    *-rc.*) CHANNEL=prerelease ;;
    *) [ "$PRERELEASE" = "1" ] || CHANNEL=stable ;;
  esac
fi

ASSET="orca-${TARGET}"
BASE="https://github.com/${REPO}/releases/download/${VERSION}"
URL_BIN="${BASE}/${ASSET}"
URL_SUM="${BASE}/${ASSET}.sha256"

echo "→ installing orca ${VERSION} (${TARGET}) to ${INSTALL_DIR}"

# ── download + verify ───────────────────────────────────────────────────────
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

curl -fsSL --proto '=https' -o "${TMP}/orca"     "$URL_BIN" \
  || die "download failed: $URL_BIN  (is the target/version correct?)"
curl -fsSL --proto '=https' -o "${TMP}/orca.sha256" "$URL_SUM" \
  || die "checksum download failed: $URL_SUM"

EXPECTED="$(awk '{print $1}' "${TMP}/orca.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "${TMP}/orca" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${TMP}/orca" | awk '{print $1}')"
else
  die "no sha256 tool available (need sha256sum or shasum)"
fi

[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch: expected $EXPECTED got $ACTUAL"

# ── install ─────────────────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR"
chmod +x "${TMP}/orca"
mv "${TMP}/orca" "${INSTALL_DIR}/orca"

if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "${INSTALL_DIR}/orca" 2>/dev/null || true
fi

# ── channel marker ──────────────────────────────────────────────────────────
# Future `orca update` reads this to know which channel to pull from. Users
# can switch later via the app; this just sets the initial pin.
ORCA_HOME="${ORCA_HOME:-$HOME/.orca}"
mkdir -p "$ORCA_HOME"
printf '%s\n' "$CHANNEL" > "${ORCA_HOME}/channel"

echo "✓ installed: ${INSTALL_DIR}/orca  (channel: ${CHANNEL})"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "  note: ${INSTALL_DIR} is not in your PATH" ;;
esac

"${INSTALL_DIR}/orca" --version 2>/dev/null || true
