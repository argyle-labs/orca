#!/usr/bin/env sh
# orca installer — detects platform, downloads matching release binary,
# verifies sha256, installs to ~/.local/bin (or $ORCA_INSTALL_DIR).
#
# Usage:
#   curl -fsSL https://github.com/scottdkey/orca/releases/latest/download/install.sh | sh
#
# Flags / env overrides:
#   --version <tag>     ORCA_VERSION       e.g. v0.0.4-rc.1 (default: latest stable)
#   --target  <triple>  ORCA_TARGET        e.g. x86_64-unknown-linux-musl (default: auto-detect)
#   --dir     <path>    ORCA_INSTALL_DIR   install directory (default: ~/.local/bin)
#   --rc, --prerelease  ORCA_PRERELEASE=1  install newest pre-release (RC); pins channel=rc
#   GITHUB_TOKEN        required — releases are private (export before running or pipe inline)
#
# Channel marker is written to $ORCA_HOME/channel ($ORCA_HOME defaults to ~/.orca).
# Valid marker values: 'stable' or 'rc' (matches the `orca update` channel enum).
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
GITHUB_TOKEN="${GITHUB_TOKEN:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version)     VERSION="$2"; shift 2 ;;
    --target)      TARGET="$2"; shift 2 ;;
    --dir)         INSTALL_DIR="$2"; shift 2 ;;
    --rc|--prerelease) PRERELEASE=1; shift ;;
    -h|--help)     sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

die() { echo "install.sh: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

[ -n "$GITHUB_TOKEN" ] || die "GITHUB_TOKEN is required (releases are private) — export GITHUB_TOKEN before running"

need curl
need chmod
need mv
need mkdir

# Authenticated GitHub API helper — returns JSON. Usage: gh_api <url> [extra curl flags...]
# Pinned API version: 2022-11-28 (current stable as of 2026-05; see
# https://docs.github.com/en/rest/about-the-rest-api/api-versions).
gh_api() {
  _url="$1"; shift
  curl -fsSL \
    -H "Authorization: Bearer ${GITHUB_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "$@" "$_url"
}

# Authenticated GitHub asset download — returns raw bytes. Usage: gh_asset <url> <output-file>
# Separate from gh_api so we send ONE Accept header (octet-stream). GitHub
# routes on the first Accept header it sees — mixing them with gh_api was the
# original source of "the .sha256 file is full of JSON" bugs.
gh_asset() {
  _url="$1"; _out="$2"
  curl -fsSL \
    -H "Authorization: Bearer ${GITHUB_TOKEN}" \
    -H "Accept: application/octet-stream" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    -o "$_out" "$_url"
}

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
      # musl detection: alpine first, then ldd reports musl, then a positive
      # glibc check via `getconf GNU_LIBC_VERSION` (works on every glibc
      # system, no globbing footguns). Default to gnu when uncertain — musl
      # binaries can run on glibc hosts but gnu binaries crash on alpine,
      # so the failure mode is asymmetric: prefer gnu.
      libc=gnu
      if [ -f /etc/alpine-release ]; then
        libc=musl
      elif command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        libc=musl
      elif command -v getconf >/dev/null 2>&1 \
           && ! getconf GNU_LIBC_VERSION >/dev/null 2>&1 \
           && ! ldd --version 2>&1 | grep -qi 'glibc\|gnu'; then
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
# Channel marker matches the `orca update` Channel enum: 'stable' or 'rc'.
CHANNEL=stable
[ "$PRERELEASE" = "1" ] && CHANNEL=rc

if [ -z "$VERSION" ]; then
  if [ "$CHANNEL" = "rc" ]; then
    # Newest prerelease — /releases is ordered newest-first; pick the first
    # tag_name containing "-rc." (stable tags never do).
    VERSION="$(gh_api "https://api.github.com/repos/${REPO}/releases?per_page=30" \
      | grep '"tag_name":' \
      | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' \
      | grep -m1 -- '-rc\.')"
    [ -n "$VERSION" ] || die "no prerelease found for ${REPO}"
  else
    # Latest stable — use the API (private repos 404 on unauthenticated /releases/latest).
    VERSION="$(gh_api "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name":' \
      | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    [ -n "$VERSION" ] || die "could not resolve latest stable (try --version or --prerelease)"
  fi
else
  # Explicit --version: infer channel from tag shape so the marker reflects
  # what the user actually installed.
  case "$VERSION" in
    *-rc.*) CHANNEL=rc ;;
    *) [ "$PRERELEASE" = "1" ] || CHANNEL=stable ;;
  esac
fi

ASSET="orca-${TARGET}"
ASSET_SUM="${ASSET}.sha256"

echo "→ installing orca ${VERSION} (${TARGET}) to ${INSTALL_DIR}"

# ── resolve asset download URLs via GitHub API ───────────────────────────────
# Private repos 404 on unauthenticated /releases/download/ URLs; the API
# asset endpoint honours the Bearer token and follows the redirect correctly
# when combined with -L and Accept: application/octet-stream.
RELEASE_JSON="$(gh_api "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")"

asset_url() {
  # GitHub's release-asset JSON serializes "url" BEFORE "name" inside each
  # asset object, then later includes the uploader's "url" inside a nested
  # "uploader" object. Walking forward from "name" grabs the uploader's URL.
  # Walk backward instead: remember the most recent "url" line and print it
  # when we see the matching "name" line.
  echo "$RELEASE_JSON" \
    | awk -v target="$1" '
        /"url":/ { last_url = $0 }
        $0 ~ "\"name\": *\"" target "\"" {
          sub(/^[^"]*"url"[[:space:]]*:[[:space:]]*"/, "", last_url)
          sub(/".*/, "", last_url)
          print last_url
          exit
        }
      '
}

URL_BIN="$(asset_url "${ASSET}")"
URL_SUM="$(asset_url "${ASSET_SUM}")"

[ -n "$URL_BIN" ] || die "asset '${ASSET}' not found in release ${VERSION} (wrong target or version?)"
[ -n "$URL_SUM" ] || die "asset '${ASSET_SUM}' not found in release ${VERSION}"

# ── download + verify ────────────────────────────────────────────────────────
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# -L (inside gh_asset) follows the redirect from the API asset URL to the S3
# pre-signed URL; Accept: application/octet-stream tells GitHub to stream raw bytes.
gh_asset "$URL_BIN" "${TMP}/orca" \
  || die "download failed: $URL_BIN  (is the target/version correct?)"
gh_asset "$URL_SUM" "${TMP}/orca.sha256" \
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
