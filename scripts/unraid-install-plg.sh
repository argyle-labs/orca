#!/usr/bin/env bash
#
# Install orca on an Unraid host via the plugin manager, without relying on a
# public release URL. Cross-compiles the Linux binary, builds a local-flavored
# .plg whose binary `<URL>` is a `file://` path on the Unraid box, scp's both
# files to /boot/config/plugins/orca/, then triggers `plugin install` over ssh
# so Unraid's plugin manager owns lifecycle (start/stop/upgrade/remove).
#
# Bootstraps the private-repo case for [[project-unraid-plugin-install-blocked-on-graphql]].
# Once Slice B / public-release path lands, the URL-driven plugin manager flow
# replaces this; until then this is how alpha + echo get .plg coverage.
#
# Usage: scripts/unraid-install-plg.sh <host> [--arch x86_64|aarch64]
#
# Required env: nothing. Assumes ssh root@<host> works (standard Unraid).

set -euo pipefail

HOST="${1:-}"
ARCH="x86_64"
PREBUILT_BIN=""
shift || true
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --binary) PREBUILT_BIN="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$HOST" ]; then
  echo "usage: $0 <host> [--arch x86_64|aarch64]" >&2
  exit 2
fi

case "$ARCH" in
  x86_64)  TRIPLE="x86_64-unknown-linux-gnu" ;;
  aarch64) TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported arch: $ARCH (expect x86_64|aarch64)" >&2; exit 2 ;;
esac

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "could not read workspace version from Cargo.toml" >&2; exit 1; }

OUT="$REPO_ROOT/dist-unraid"
mkdir -p "$OUT"

if [ -n "$PREBUILT_BIN" ]; then
  [ -x "$PREBUILT_BIN" ] || { echo "prebuilt binary missing or not executable: $PREBUILT_BIN" >&2; exit 1; }
  echo "→ using prebuilt binary $PREBUILT_BIN (skipping cross-compile)"
else
  echo "→ building orca for $TRIPLE (version $VERSION)"
  # Linux targets cross-compile via zigbuild from any host; matches the
  # pattern in scripts/release-lib.sh::build_orca_targets.
  command -v cargo-zigbuild >/dev/null \
    || { echo "cargo-zigbuild missing — cargo install cargo-zigbuild + brew install zig" >&2; exit 1; }
  cargo zigbuild --release --target "$TRIPLE" -p server
fi

# Also build a host-native binary so the .plg generator picks up any
# package.rs script changes from this working tree — the installed
# ~/.local/bin/orca is frequently stale during iteration on the install
# scripts themselves.
HOST_TRIPLE="$(rustc -vV | awk '/^host:/ {print $2}')"
echo "→ building host orca for $HOST_TRIPLE (template-render only)"
cargo build --release --target "$HOST_TRIPLE" -p server
if [ -n "$PREBUILT_BIN" ]; then
  BIN_SRC="$PREBUILT_BIN"
else
  BIN_SRC="$REPO_ROOT/target/$TRIPLE/release/orca"
fi
[ -x "$BIN_SRC" ] || { echo "binary missing: $BIN_SRC" >&2; exit 1; }
cp "$BIN_SRC" "$OUT/orca"

# Build a local-flavored .plg: binary URL points at the path on the Unraid box
# where we'll scp the binary BEFORE invoking `plugin install`. The plugin
# manager reads the file://, verifies the MD5, and runs the install script.
# Use the host-native orca binary (PATH or ~/.local/bin) to run `system build`
# — the cross-compiled Linux binary at $BIN_SRC can't execute on macOS, but
# the .plg generator only needs to hash + template-write, so any host binary
# works.
LOCAL_BIN_PATH="/boot/config/plugins/orca/bin/orca"
HOST_ORCA="$REPO_ROOT/target/$HOST_TRIPLE/release/orca"
[ -x "$HOST_ORCA" ] || { echo "host orca build output missing: $HOST_ORCA" >&2; exit 1; }
echo "→ generating .plg with file:// binary URL (using $HOST_ORCA)"
"$HOST_ORCA" system build \
  --format plg \
  --binary "$OUT/orca" \
  --arch "$ARCH" \
  --plg-binary-url "file://$LOCAL_BIN_PATH" \
  --plg-url "file:///boot/config/plugins/orca.plg" \
  --out-dir "$OUT"

PLG="$OUT/orca.plg"
[ -f "$PLG" ] || { echo "plg missing: $PLG" >&2; exit 1; }

echo "→ stop running orca daemon on $HOST (Text file busy guard)"
# Without this, writing /etc/rc.d/rc.orca fails with EBUSY because the
# running daemon binary is the supervisor target. Idempotent — no-op when
# nothing is running.
ssh "root@$HOST" "/etc/rc.d/rc.orca stop 2>/dev/null || true; \
  pkill -x orca 2>/dev/null || true"

echo "→ stage .plg + remove any prior install on $HOST"
# Stage the .plg in /tmp so it survives `plugin remove` (which moves
# /boot/config/plugins/orca.plg to plugins-removed/ AND our remove script
# does `rm -rf /boot/config/plugins/orca`). Removing FIRST, then scp'ing
# the binary, ensures the binary file:// URL is still present when
# `plugin install` runs.
scp "$PLG" "root@$HOST:/tmp/orca.plg"
ssh "root@$HOST" "plugin remove orca.plg 2>/dev/null || true"

echo "→ scp binary to $HOST"
ssh "root@$HOST" "mkdir -p /boot/config/plugins/orca/bin"
scp "$OUT/orca" "root@$HOST:$LOCAL_BIN_PATH"
ssh "root@$HOST" "chmod 0755 $LOCAL_BIN_PATH"

echo "→ plugin install on $HOST"
ssh "root@$HOST" "plugin install /tmp/orca.plg"

echo "✓ orca installed on $HOST via .plg (version $VERSION)"
echo "  verify: ssh root@$HOST \"ss -tlnp | grep -E ':(12000|12002|12443)'\""
