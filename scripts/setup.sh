#!/usr/bin/env bash
# Verify (and where possible install) the toolchain needed to build orca.
# Supported hosts: macOS (brew), Arch / CachyOS (pacman), CI runners.
#
# Usage:
#   bash scripts/setup.sh           — check + auto-install via package manager
#   bash scripts/setup.sh --check   — check only; never install
#   bash scripts/setup.sh --ci      — skip OS-package installs (CI bootstraps via setup actions)
#
# Exit codes:
#   0 — all required tools present (or installed) and pass version requirements
#   1 — missing tools that the script could not install automatically

set -euo pipefail

MODE="install"
case "${1:-}" in
  --check) MODE="check" ;;
  --ci)    MODE="ci" ;;
  "")      ;;
  *) echo "unknown arg: $1" >&2; exit 2 ;;
esac

OS=""
PKG=""
case "$(uname -s)" in
  Darwin) OS="macos"; PKG="brew" ;;
  Linux)
    OS="linux"
    if   [[ -f /etc/arch-release || -f /etc/cachyos-release ]]; then PKG="pacman"
    elif [[ -f /etc/debian_version ]];                            then PKG="apt"
    else                                                                PKG="unknown"
    fi
    ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 2 ;;
esac

# ── package map: tool → (macos brew pkg, arch pacman pkg, debian apt pkg) ─────
# For tools that must be present but where install is skipped (e.g. rustup is
# not in pacman main repo), the install column is empty and we only print a
# manual hint.
declare -A BREW_PKG=(
  [cc]=""             # provided by Xcode CLT
  [make]=""           # Xcode CLT
  [perl]=""           # macOS system perl
  [pkg-config]="pkg-config"
  [rustup]="rustup"
  [node]="node"
  [npm]=""            # bundled with node
  [go]="go"
  [java]="openjdk@21"
  [gradle]="gradle"
)
declare -A PACMAN_PKG=(
  [cc]="base-devel"
  [make]="base-devel"
  [perl]="perl"
  [pkg-config]="base-devel"
  [rustup]="rustup"
  [node]="nodejs"
  [npm]="npm"
  [go]="go"
  [java]="jdk21-openjdk"
  [gradle]="gradle"
)
declare -A APT_PKG=(
  [cc]="build-essential"
  [make]="build-essential"
  [perl]="perl"
  [pkg-config]="pkg-config"
  [rustup]=""         # use rustup.rs installer
  [node]="nodejs"
  [npm]="npm"
  [go]="golang-go"
  [java]="openjdk-21-jdk"
  [gradle]="gradle"
)

MISSING=()
WARN=()

have() { command -v "$1" >/dev/null 2>&1; }

# Compare two semver-ish strings; returns 0 if $1 >= $2.
ge() {
  [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" == "$2" ]]
}

check_tool() {
  local name="$1" probe_cmd="$2" min_ver="${3:-}"
  if ! have "$name" && [[ -z "$probe_cmd" ]]; then MISSING+=("$name"); return; fi
  if ! eval "$probe_cmd" >/dev/null 2>&1; then MISSING+=("$name"); return; fi
  if [[ -n "$min_ver" ]]; then
    local actual
    actual=$(eval "$probe_cmd" 2>/dev/null | head -1 | grep -oE '[0-9]+(\.[0-9]+){1,2}' | head -1 || echo 0)
    if ! ge "$actual" "$min_ver"; then
      WARN+=("$name $actual < $min_ver (recommended)")
    fi
  fi
}

# ── checks ────────────────────────────────────────────────────────────────────
check_tool cc          "cc --version"
check_tool make        "make --version"
check_tool perl        "perl --version"
check_tool pkg-config  "pkg-config --version"
check_tool rustup      "rustup --version"
check_tool cargo       "cargo --version"   1.95.0
check_tool node        "node --version"    22.0.0
check_tool npm         "npm --version"
check_tool go          "go version"        1.26.0
check_tool java        "java -version 2>&1" 21
check_tool gradle      "gradle --version | grep '^Gradle '" 8

if [[ ${#MISSING[@]} -eq 0 && ${#WARN[@]} -eq 0 ]]; then
  echo "✓ all build prerequisites present"
  exit 0
fi

echo "── build prerequisite check ──"
[[ ${#MISSING[@]} -gt 0 ]] && printf '  missing: %s\n' "${MISSING[@]}"
[[ ${#WARN[@]}    -gt 0 ]] && printf '  warn:    %s\n' "${WARN[@]}"
echo

if [[ "$MODE" == "check" || "$MODE" == "ci" ]]; then
  echo "(check-only mode — skipping install)"
  [[ ${#MISSING[@]} -gt 0 ]] && exit 1 || exit 0
fi

# ── install ───────────────────────────────────────────────────────────────────
install_one() {
  local tool="$1" pkg=""
  case "$PKG" in
    brew)   pkg="${BREW_PKG[$tool]:-}" ;;
    pacman) pkg="${PACMAN_PKG[$tool]:-}" ;;
    apt)    pkg="${APT_PKG[$tool]:-}" ;;
  esac
  if [[ -z "$pkg" ]]; then
    echo "  $tool — no auto-install available; install manually:"
    case "$tool" in
      cc|make) echo "    macOS: xcode-select --install" ;;
      rustup)  echo "    https://rustup.rs/" ;;
    esac
    return 1
  fi
  echo "  → installing $tool ($pkg)"
  case "$PKG" in
    brew)   brew install "$pkg" ;;
    pacman) sudo pacman -S --needed --noconfirm $pkg ;;
    apt)    sudo apt-get install -y $pkg ;;
  esac
}

if [[ "$PKG" == "unknown" ]]; then
  echo "unknown Linux distro — install manually: ${MISSING[*]}" >&2
  exit 1
fi

failed=0
for tool in "${MISSING[@]}"; do
  install_one "$tool" || failed=1
done

# Toolchain pinned by rust-toolchain.toml; rustup auto-installs on first cargo invoke,
# but pre-pull it so subsequent commands don't pause. Targets only added here for
# local dev convenience — CI installs them per matrix entry.
if have rustup; then
  rustup show active-toolchain >/dev/null 2>&1 || rustup show >/dev/null
fi

[[ $failed -eq 0 ]] || { echo "some prerequisites could not be auto-installed (see above)"; exit 1; }
echo "✓ setup complete"
