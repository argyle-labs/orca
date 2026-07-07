#!/usr/bin/env sh
# orca installer — detects platform, downloads matching release binary,
# verifies sha256, installs to ~/.local/bin (or $ORCA_INSTALL_DIR).
#
# Usage:
#   curl -fsSL https://github.com/argyle-labs/orca/releases/latest/download/install.sh | sh
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
#   --dev-setup          ORCA_DEV_SETUP=1     install Rust toolchain + cargo-watch for dev mode
#                                             (installs build deps via apt/apk as needed)
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

REPO="argyle-labs/orca"
VERSION="${ORCA_VERSION:-}"
TARGET="${ORCA_TARGET:-}"
INSTALL_DIR="${ORCA_INSTALL_DIR:-}"
PRERELEASE="${ORCA_PRERELEASE:-0}"
GITHUB_TOKEN="${GITHUB_TOKEN:-}"
FROM_FILE="${ORCA_FROM_FILE:-}"
SKIP_SHA="${ORCA_SKIP_SHA:-0}"
ADMIN_PUBKEY="${ORCA_ADMIN_PUBKEY:-}"
DEV_SETUP="${ORCA_DEV_SETUP:-0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version)         VERSION="$2"; shift 2 ;;
    --target)          TARGET="$2"; shift 2 ;;
    --dir)             INSTALL_DIR="$2"; shift 2 ;;
    --rc|--prerelease) PRERELEASE=1; shift ;;
    --from-file)       FROM_FILE="$2"; shift 2 ;;
    --skip-sha)        SKIP_SHA=1; shift ;;
    --admin-pubkey)    ADMIN_PUBKEY="$2"; shift 2 ;;
    --dev-setup)       DEV_SETUP=1; shift ;;
    -h|--help)         sed -n '2,32p' "$0" 2>/dev/null || echo "see scripts/install.sh header"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

die() { echo "install.sh: $*" >&2; exit 1; }
warn() { echo "install.sh: warning: $*" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

# Dev-setup-only mode: install build deps + rustup + cargo-watch, then exit.
# Triggered by ORCA_DEV_SETUP_ONLY=1 (set by root-mode --dev-setup re-invocation).
if [ "${ORCA_DEV_SETUP_ONLY:-0}" = "1" ]; then
  DEV_SETUP=1
fi

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
# When running as root we install for the `orca` service user.
# User creation, group assignment, SSH keys, and linger are handled by
# `orca system bootstrap` — the binary is the single source of that logic so
# it stays consistent across install.sh, deploy-host.sh, and package postinst.
ORCA_USER="orca"
ORCA_HOME_DIR="/var/lib/orca"
ORCA_SERVICE_BIN="${ORCA_HOME_DIR}/.local/bin/orca"

# ── privilege escalation for service-user installs ──────────────────────────
# When the controller deploys as a non-root login user (e.g. `ssh user@host`)
# but this host runs orca as the `orca` service user, a plain non-root install
# would land in the login user's $HOME and leave the daemon stale (the bug that
# stranded the rc.11 fleet rollout). Detect that case — `--admin-pubkey` was
# passed (controller deploy intent) or a daemon binary already exists at the
# service path — and re-exec under sudo so the install targets /var/lib/orca
# and can restart the system service. ORCA_DEV_SETUP_ONLY re-invocations run as
# the orca user by design and must not escalate.
if [ "$(id -u)" != 0 ] \
   && [ "${ORCA_DEV_SETUP_ONLY:-0}" != "1" ] \
   && { [ -n "$ADMIN_PUBKEY" ] || [ -x "$ORCA_SERVICE_BIN" ]; }; then
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    warn "service install detected (daemon at $ORCA_SERVICE_BIN or --admin-pubkey set) — re-executing under sudo to target the service user"
    exec sudo env \
      ORCA_VERSION="$VERSION" \
      ORCA_TARGET="$TARGET" \
      ORCA_INSTALL_DIR="$INSTALL_DIR" \
      ORCA_PRERELEASE="$PRERELEASE" \
      GITHUB_TOKEN="$GITHUB_TOKEN" \
      ORCA_FROM_FILE="$FROM_FILE" \
      ORCA_SKIP_SHA="$SKIP_SHA" \
      ORCA_ADMIN_PUBKEY="$ADMIN_PUBKEY" \
      ORCA_DEV_SETUP="$DEV_SETUP" \
      sh "$0"
  else
    die "service daemon detected at $ORCA_SERVICE_BIN but not running as root and passwordless sudo is unavailable — re-run as root (ssh root@host) or grant sudo"
  fi
fi

if [ "$(id -u)" = 0 ]; then
  warn "running as root — installing for service user '$ORCA_USER'"
  INSTALL_DIR="${INSTALL_DIR:-$ORCA_HOME_DIR/.local/bin}"
  ORCA_HOME_TARGET="$ORCA_HOME_DIR/.orca"
  RUN_AS_ORCA=1
else
  INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
  ORCA_HOME_TARGET="${ORCA_HOME:-$HOME/.orca}"
  RUN_AS_ORCA=0
fi

# ── dev setup: Rust toolchain + cargo-watch ─────────────────────────────────
# Installs build dependencies for the current distro, then rustup + cargo-watch
# into the target user's home (ORCA_HOME_TARGET or $HOME).
dev_setup() {
  _home="${1:-$HOME}"
  _cargo="${_home}/.cargo/bin/cargo"

  echo "→ dev-setup: installing build dependencies"
  if [ -f /etc/alpine-release ]; then
    apk add --no-cache build-base curl openssl-dev pkgconf 2>/dev/null \
      || warn "apk add failed — build tools may be incomplete"
  elif [ -f /etc/debian_version ]; then
    apt-get install -y build-essential curl pkg-config libssl-dev 2>/dev/null \
      || warn "apt-get failed — build tools may be incomplete"
  elif [ -f /etc/os-release ] && grep -qi 'unraid\|slackware' /etc/os-release 2>/dev/null; then
    warn "Unraid/Slackware detected — skipping automatic build-tools install; ensure gcc is available"
  else
    warn "unknown distro — skipping build-tools install; ensure gcc and libssl-dev are available"
  fi

  if [ -x "$_cargo" ]; then
    echo "→ dev-setup: rustup already installed at ${_home}/.cargo"
  else
    echo "→ dev-setup: installing rustup"
    if command -v curl >/dev/null 2>&1; then
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain stable
    elif command -v wget >/dev/null 2>&1; then
      wget -qO- https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain stable
    else
      warn "neither curl nor wget found — cannot install rustup"
      return 1
    fi
  fi

  if [ -x "${_home}/.cargo/bin/cargo-watch" ]; then
    echo "✓ dev-setup: cargo-watch already installed"
  else
    echo "→ dev-setup: installing cargo-watch"
    "${_home}/.cargo/bin/cargo" install cargo-watch \
      && echo "✓ dev-setup: cargo-watch installed" \
      || warn "cargo-watch install failed"
  fi
}

# Early exit for dev-setup-only invocations (root re-invokes as orca user).
if [ "${ORCA_DEV_SETUP_ONLY:-0}" = "1" ]; then
  dev_setup "$HOME"
  exit $?
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

  _asset_ver="${VERSION#v}"
  ASSET="orca-${_asset_ver}-${TARGET}"
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

# Service-user creation (user, group, linger, SSH key) is no longer a separate
# step: it was folded into `orca system install --service-user` (invoked below,
# after the binary is in place). The former standalone `system bootstrap`
# subcommand was removed, so calling it here only errored. See the RUN_AS_ORCA
# block further down.
mkdir -p "$INSTALL_DIR"
chmod +x "${TMP}/orca"
mv "${TMP}/orca" "${INSTALL_DIR}/orca"

if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "${INSTALL_DIR}/orca" 2>/dev/null || true
  # Ad-hoc sign so Gatekeeper accepts the binary. Idempotent — re-signs an
  # already-signed binary too, which matters for cross-built linux→macOS
  # releases that didn't get signed on the build host. Without this, the
  # daemon gets SIGKILLed on first launch and launchctl reports exit -9.
  codesign --force --sign - "${INSTALL_DIR}/orca" 2>/dev/null || true
fi

# Bounce the running daemon onto the new binary. kill-stale (above) already
# killed it; this restarts whichever supervisor owns it (launchd on macOS,
# systemd-user on Linux). Idempotent and silent if no supervisor is loaded.
restart_orca_service() {
  case "$(uname -s)" in
    Darwin)
      if launchctl list 2>/dev/null | grep -q com.orca.daemon; then
        launchctl kickstart -k "gui/$(id -u)/com.orca.daemon" 2>/dev/null \
          && echo "✓ daemon restarted (launchd)"
      fi
      ;;
    Linux)
      if command -v systemctl >/dev/null 2>&1 \
         && systemctl --user is-enabled orca.service >/dev/null 2>&1; then
        systemctl --user restart orca.service 2>/dev/null \
          && echo "✓ daemon restarted (systemd --user)"
      fi
      ;;
  esac
}
restart_orca_service

mkdir -p "$ORCA_HOME_TARGET"
printf '%s\n' "$CHANNEL" > "${ORCA_HOME_TARGET}/channel"

# Hand the tree over to the orca user when running as root, then run
# `orca daemon install --service-user orca` AS ROOT (not via runuser): the
# binary itself detects the init system (systemd / openrc / unraid) and
# writes the appropriate system-level unit. PKI dir is created + chowned
# by daemon install.
if [ "$RUN_AS_ORCA" = "1" ]; then
  echo "✓ installed: ${INSTALL_DIR}/orca  (channel: ${CHANNEL}, user: ${ORCA_USER})"
  # Create the service user + group, then install/refresh the daemon supervisor.
  # This MUST precede the chown below: the user/group it creates is what the
  # chown targets (on a fresh host neither exists yet). `--admin-pubkey` installs
  # the SSH key when provided.
  echo "→ bootstrapping daemon as ${ORCA_USER} via system service"
  if [ -n "$ADMIN_PUBKEY" ]; then
    "${INSTALL_DIR}/orca" system install --service-user "$ORCA_USER" --admin-pubkey "$ADMIN_PUBKEY" \
      || warn "daemon install failed — re-run: ${INSTALL_DIR}/orca system install --service-user $ORCA_USER"
  else
    "${INSTALL_DIR}/orca" system install --service-user "$ORCA_USER" \
      || warn "daemon install failed — re-run: ${INSTALL_DIR}/orca system install --service-user $ORCA_USER"
  fi
  # Now the user + group exist; hand the tree over.
  chown -R "$ORCA_USER" "$ORCA_HOME_DIR/.local" "$ORCA_HOME_TARGET" 2>/dev/null \
    || warn "chown to ${ORCA_USER} failed — check service user/group exist"
  # System-wide symlink so any user on the box can invoke `orca` from PATH.
  # The binary itself reads $HOME/.orca for state, so non-orca users get
  # their own (empty) state; daemon/state operations still need
  # `sudo -u $ORCA_USER orca …`.
  if [ -d /usr/local/bin ] && [ ! -e /usr/local/bin/orca ]; then
    ln -sf "${INSTALL_DIR}/orca" /usr/local/bin/orca \
      && echo "✓ symlinked /usr/local/bin/orca → ${INSTALL_DIR}/orca"
  fi
  # Restart the service so it picks up the new binary instead of running the
  # old (now-deleted) inode kill-stale terminated above. Detects systemd,
  # openrc, and unraid rc scripts — silent no-op if none match.
  if command -v systemctl >/dev/null 2>&1 && systemctl is-enabled orca.service >/dev/null 2>&1; then
    systemctl restart orca.service 2>/dev/null && echo "✓ daemon restarted (systemd)"
  elif command -v rc-service >/dev/null 2>&1 && rc-service -e orca >/dev/null 2>&1; then
    rc-service orca restart 2>/dev/null && echo "✓ daemon restarted (openrc)"
  elif [ -x /etc/rc.d/rc.orca ]; then
    /etc/rc.d/rc.orca restart >/dev/null 2>&1 && echo "✓ daemon restarted (unraid)"
  fi
  if [ "$DEV_SETUP" = "1" ]; then
    # Re-invoke this script as the orca user in dev-setup-only mode so
    # rustup lands in their home (~/.cargo, ~/.rustup).
    ORCA_DEV_SETUP_ONLY=1 su -s /bin/sh "$ORCA_USER" -c "sh '$0'" \
      || warn "dev-setup failed — re-run: ORCA_DEV_SETUP_ONLY=1 su -s /bin/sh $ORCA_USER -c 'sh install.sh'"
  fi
  exit 0
fi

echo "✓ installed: ${INSTALL_DIR}/orca  (channel: ${CHANNEL})"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "  note: ${INSTALL_DIR} is not in your PATH" ;;
esac

if [ "$DEV_SETUP" = "1" ]; then
  dev_setup "$HOME"
fi

"${INSTALL_DIR}/orca" --version 2>/dev/null || true
