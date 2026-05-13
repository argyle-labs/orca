#!/usr/bin/env sh
# orca installer — detects platform, downloads matching release binary,
# verifies sha256, installs to ~/.local/bin (or $ORCA_INSTALL_DIR).
#
# Usage:
#   curl -fsSL https://github.com/scottdkey/orca/releases/latest/download/install.sh | sh
#
# Flags / env overrides:
#   --version <tag>      ORCA_VERSION         e.g. v0.0.4-rc.1 (default: latest stable)
#   --target  <triple>   ORCA_TARGET          e.g. x86_64-unknown-linux-musl (default: auto-detect)
#   --dir     <path>     ORCA_INSTALL_DIR     install directory (default: ~/.local/bin)
#   --rc, --prerelease   ORCA_PRERELEASE=1    install newest pre-release (RC); pins channel=rc
#   --from-file <path>   ORCA_FROM_FILE       skip GitHub fetch; install this local binary instead
#                                             (use with a sibling <file>.sha256 or set --skip-sha)
#   --skip-sha           ORCA_SKIP_SHA=1      skip sha256 verification (push mode w/ pre-verified bytes)
#   --admin-pubkey <key> ORCA_ADMIN_PUBKEY    SSH pubkey to install for the orca service user
#                                             — REQUIRED when running as root and orca user is new
#   GITHUB_TOKEN         required for download mode — releases are private
#
# Root-mode auto-bootstrap:
#   When invoked as root, install.sh creates a least-privileged `orca` service
#   user (home /var/lib/orca, groups docker+systemd-journal best-effort, NO sudo)
#   and installs the binary into that user's home. Lingering is enabled so the
#   user-systemd session persists without an active login. Root SSH keys are
#   NEVER copied — orca's authorized_keys come from --admin-pubkey only.
#
# Channel marker is written to $ORCA_HOME/channel ($ORCA_HOME defaults to ~/.orca).
# Valid marker values: 'stable' or 'rc' (matches the `orca update` channel enum).
#
# Examples:
#   sh install.sh
#   sh install.sh --version v0.0.3-rc.4
#   sh install.sh --target x86_64-unknown-linux-musl
#   sh install.sh --from-file /tmp/orca --skip-sha          # push install
#   sh install.sh --admin-pubkey "ssh-ed25519 AAAA... me"   # root-mode

set -eu

REPO="scottdkey/orca"
VERSION="${ORCA_VERSION:-}"
TARGET="${ORCA_TARGET:-}"
INSTALL_DIR="${ORCA_INSTALL_DIR:-}"
PRERELEASE="${ORCA_PRERELEASE:-0}"
GITHUB_TOKEN="${GITHUB_TOKEN:-}"
FROM_FILE="${ORCA_FROM_FILE:-}"
SKIP_SHA="${ORCA_SKIP_SHA:-0}"
ADMIN_PUBKEY="${ORCA_ADMIN_PUBKEY:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version)         VERSION="$2"; shift 2 ;;
    --target)          TARGET="$2"; shift 2 ;;
    --dir)             INSTALL_DIR="$2"; shift 2 ;;
    --rc|--prerelease) PRERELEASE=1; shift ;;
    --from-file)       FROM_FILE="$2"; shift 2 ;;
    --skip-sha)        SKIP_SHA=1; shift ;;
    --admin-pubkey)    ADMIN_PUBKEY="$2"; shift 2 ;;
    -h|--help)         sed -n '2,32p' "$0" 2>/dev/null || echo "see scripts/install.sh header"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

die() { echo "install.sh: $*" >&2; exit 1; }
warn() { echo "install.sh: warning: $*" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

need chmod
need mv
need mkdir

# ── download abstraction (device-agnostic: curl preferred, wget fallback) ────
# Two callable shapes:
#   http_get_json   <url>            → prints JSON to stdout
#   http_get_asset  <url> <out>      → writes octet-stream bytes to file
# Both add the GitHub auth + API-version headers when GITHUB_TOKEN is set.
HTTP_TOOL=""
if command -v curl >/dev/null 2>&1; then
  HTTP_TOOL=curl
elif command -v wget >/dev/null 2>&1; then
  HTTP_TOOL=wget
fi

http_get_json() {
  _url="$1"
  case "$HTTP_TOOL" in
    curl)
      curl -fsSL \
        -H "Authorization: Bearer ${GITHUB_TOKEN}" \
        -H "Accept: application/vnd.github+json" \
        -H "X-GitHub-Api-Version: 2022-11-28" \
        "$_url"
      ;;
    wget)
      wget -qO- \
        --header="Authorization: Bearer ${GITHUB_TOKEN}" \
        --header="Accept: application/vnd.github+json" \
        --header="X-GitHub-Api-Version: 2022-11-28" \
        "$_url"
      ;;
    *) die "need curl or wget to fetch from GitHub (use --from-file for push install)" ;;
  esac
}

http_get_asset() {
  _url="$1"; _out="$2"
  case "$HTTP_TOOL" in
    curl)
      curl -fsSL \
        -H "Authorization: Bearer ${GITHUB_TOKEN}" \
        -H "Accept: application/octet-stream" \
        -H "X-GitHub-Api-Version: 2022-11-28" \
        -o "$_out" "$_url"
      ;;
    wget)
      wget -qO "$_out" \
        --header="Authorization: Bearer ${GITHUB_TOKEN}" \
        --header="Accept: application/octet-stream" \
        --header="X-GitHub-Api-Version: 2022-11-28" \
        "$_url"
      ;;
    *) die "need curl or wget (use --from-file for push install)" ;;
  esac
}

# ── root-mode bootstrap ─────────────────────────────────────────────────────
# When running as root we create the `orca` service user and install for them.
# This is the only branch that mutates /etc/passwd or /var/lib. Idempotent.
ORCA_USER="orca"
ORCA_HOME_DIR="/var/lib/orca"

ensure_orca_user() {
  if id "$ORCA_USER" >/dev/null 2>&1; then
    return 0
  fi
  [ -n "$ADMIN_PUBKEY" ] || die "running as root with no orca user — pass --admin-pubkey \"\$(cat ~/.ssh/id_ed25519.pub)\" so the controller can ssh in later"

  # Pick a shell that actually exists. Alpine/busybox systems often have no
  # /bin/bash and useradd's default '-s /bin/bash' fails with a warning + the
  # subsequent `su - orca` blowing up. Prefer bash when present, fall back to sh.
  ORCA_SHELL=/bin/sh
  [ -x /bin/bash ] && ORCA_SHELL=/bin/bash

  warn "creating system user '$ORCA_USER' (home $ORCA_HOME_DIR, shell $ORCA_SHELL, no sudo)"
  if command -v useradd >/dev/null 2>&1; then
    useradd \
      --system \
      --create-home \
      --home-dir "$ORCA_HOME_DIR" \
      --shell "$ORCA_SHELL" \
      "$ORCA_USER"
  elif command -v adduser >/dev/null 2>&1; then
    # busybox adduser (Alpine). -S = system user, -D = no password, -H would skip
    # home creation — we want home, so omit -H. Shell + home-dir are positional flags.
    adduser -S -D -h "$ORCA_HOME_DIR" -s "$ORCA_SHELL" "$ORCA_USER"
  else
    die "neither useradd nor adduser found — cannot create $ORCA_USER"
  fi

  # Best-effort group adds. Skip silently if the group doesn't exist.
  for grp in docker systemd-journal; do
    if getent group "$grp" >/dev/null 2>&1; then
      if command -v usermod >/dev/null 2>&1; then
        usermod -aG "$grp" "$ORCA_USER" || warn "could not add $ORCA_USER to $grp"
      elif command -v addgroup >/dev/null 2>&1; then
        addgroup "$ORCA_USER" "$grp" || warn "could not add $ORCA_USER to $grp"
      fi
    fi
  done

  # Linger so the user-systemd session survives without an interactive login.
  # Only meaningful on systemd hosts; harmless to skip elsewhere.
  if command -v loginctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    loginctl enable-linger "$ORCA_USER" || warn "loginctl enable-linger failed"
  fi
  install_orca_ssh_key
}

install_orca_ssh_key() {
  # orca gets its OWN authorized_keys — never inherits root's keys.
  _ssh_dir="$ORCA_HOME_DIR/.ssh"
  _auth="$_ssh_dir/authorized_keys"
  mkdir -p "$_ssh_dir"
  printf '%s\n' "$ADMIN_PUBKEY" > "$_auth"
  chmod 700 "$_ssh_dir"
  chmod 600 "$_auth"
  chown -R "$ORCA_USER" "$_ssh_dir"
}

# When we end up running as root, set install paths under orca's home.
if [ "$(id -u)" = 0 ]; then
  warn "running as root — installing for service user '$ORCA_USER' instead"
  ensure_orca_user
  INSTALL_DIR="${INSTALL_DIR:-$ORCA_HOME_DIR/.local/bin}"
  ORCA_HOME_TARGET="$ORCA_HOME_DIR/.orca"
  RUN_AS_ORCA=1
else
  INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
  ORCA_HOME_TARGET="${ORCA_HOME:-$HOME/.orca}"
  RUN_AS_ORCA=0
fi

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

# ── resolve version + source bytes ──────────────────────────────────────────
CHANNEL=stable
[ "$PRERELEASE" = "1" ] && CHANNEL=rc

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ -n "$FROM_FILE" ]; then
  # ── push mode: bytes already on disk, no GitHub roundtrip ─────────────────
  [ -f "$FROM_FILE" ] || die "--from-file: not found: $FROM_FILE"
  cp "$FROM_FILE" "${TMP}/orca"
  if [ "$SKIP_SHA" != "1" ]; then
    [ -f "${FROM_FILE}.sha256" ] || die "expected ${FROM_FILE}.sha256 next to --from-file (or pass --skip-sha)"
    cp "${FROM_FILE}.sha256" "${TMP}/orca.sha256"
  fi
  # Version is informational only in push mode. If caller didn't pass --version,
  # we can't infer it without running the binary first — fall back to "unknown".
  VERSION="${VERSION:-unknown}"
  echo "→ installing orca ${VERSION} (${TARGET}, from-file) to ${INSTALL_DIR}"
else
  # ── pull mode: fetch from GitHub releases ─────────────────────────────────
  [ -n "$GITHUB_TOKEN" ] || die "GITHUB_TOKEN is required for download mode (export GITHUB_TOKEN, or use --from-file)"
  [ -n "$HTTP_TOOL" ] || die "no http tool found (need curl or wget) — install one, or use --from-file"

  if [ -z "$VERSION" ]; then
    if [ "$CHANNEL" = "rc" ]; then
      VERSION="$(http_get_json "https://api.github.com/repos/${REPO}/releases?per_page=30" \
        | grep '"tag_name":' \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' \
        | grep -m1 -- '-rc\.')"
      [ -n "$VERSION" ] || die "no prerelease found for ${REPO}"
    else
      VERSION="$(http_get_json "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name":' \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
      [ -n "$VERSION" ] || die "could not resolve latest stable (try --version or --prerelease)"
    fi
  else
    case "$VERSION" in
      *-rc.*) CHANNEL=rc ;;
      *) [ "$PRERELEASE" = "1" ] || CHANNEL=stable ;;
    esac
  fi

  ASSET="orca-${TARGET}"
  ASSET_SUM="${ASSET}.sha256"

  echo "→ installing orca ${VERSION} (${TARGET}) to ${INSTALL_DIR}"

  RELEASE_JSON="$(http_get_json "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")"
  asset_url() {
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
  [ -n "$URL_BIN" ] || die "asset '${ASSET}' not found in release ${VERSION}"
  [ -n "$URL_SUM" ] || die "asset '${ASSET_SUM}' not found in release ${VERSION}"

  http_get_asset "$URL_BIN" "${TMP}/orca" \
    || die "download failed: $URL_BIN"
  http_get_asset "$URL_SUM" "${TMP}/orca.sha256" \
    || die "checksum download failed: $URL_SUM"
fi

# ── verify ──────────────────────────────────────────────────────────────────
if [ "$SKIP_SHA" != "1" ]; then
  EXPECTED="$(awk '{print $1}' "${TMP}/orca.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "${TMP}/orca" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    ACTUAL="$(shasum -a 256 "${TMP}/orca" | awk '{print $1}')"
  else
    die "no sha256 tool available (need sha256sum or shasum)"
  fi
  [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch: expected $EXPECTED got $ACTUAL"
fi

# ── install ─────────────────────────────────────────────────────────────────
# Kill stale runtime processes (mcp-serve, daemon) holding the old binary's
# inode open. Uses the EXISTING binary's `system kill-stale` so the patterns
# stay single-source in projects/server/src/commands/system.rs.
[ -x "${INSTALL_DIR}/orca" ] && "${INSTALL_DIR}/orca" system kill-stale 2>/dev/null || true

mkdir -p "$INSTALL_DIR"
chmod +x "${TMP}/orca"
mv "${TMP}/orca" "${INSTALL_DIR}/orca"

if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "${INSTALL_DIR}/orca" 2>/dev/null || true
fi

mkdir -p "$ORCA_HOME_TARGET"
printf '%s\n' "$CHANNEL" > "${ORCA_HOME_TARGET}/channel"

# Hand the tree over to the orca user when running as root, then run
# `orca daemon install --service-user orca` AS ROOT (not via runuser): the
# binary itself detects the init system (systemd / openrc / unraid) and
# writes the appropriate system-level unit. PKI dir is created + chowned
# by daemon install.
if [ "$RUN_AS_ORCA" = "1" ]; then
  chown -R "$ORCA_USER" "$ORCA_HOME_DIR/.local" "$ORCA_HOME_TARGET"
  # System-wide symlink so any user on the box can invoke `orca` from PATH.
  # The binary itself reads $HOME/.orca for state, so non-orca users get
  # their own (empty) state; daemon/state operations still need
  # `sudo -u $ORCA_USER orca …`.
  if [ -d /usr/local/bin ] && [ ! -e /usr/local/bin/orca ]; then
    ln -sf "${INSTALL_DIR}/orca" /usr/local/bin/orca \
      && echo "✓ symlinked /usr/local/bin/orca → ${INSTALL_DIR}/orca"
  fi
  echo "✓ installed: ${INSTALL_DIR}/orca  (channel: ${CHANNEL}, user: ${ORCA_USER})"
  echo "→ bootstrapping daemon as ${ORCA_USER} via system service"
  "${INSTALL_DIR}/orca" daemon install --service-user "$ORCA_USER" \
    || warn "daemon install failed — re-run: ${INSTALL_DIR}/orca daemon install --service-user $ORCA_USER"
  exit 0
fi

echo "✓ installed: ${INSTALL_DIR}/orca  (channel: ${CHANNEL})"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "  note: ${INSTALL_DIR} is not in your PATH" ;;
esac

"${INSTALL_DIR}/orca" --version 2>/dev/null || true
