#!/usr/bin/env bash
# CI-exact coverage inside a Linux container (matches .github/workflows/ci.yml).
# macOS builds skip Linux-gated code, so local `cargo llvm-cov` overstates the
# CI number by ~3 points — this reproduces the real Linux line set.
#
# Usage: scripts/linux-coverage.sh [/path/to/worktree]
# Prints the per-file report to stdout and the TOTAL line at the end.
set -euo pipefail

WT="${1:-$(pwd)}"
IMG="rust:1.95-bookworm"

# Named volumes keep the cargo registry + a Linux target dir warm across runs,
# so only the first build is slow. Target dir is container-local (NOT the host
# target/, which holds macOS artifacts).
docker run --rm \
  -v "${WT}:/src" \
  -v orca-cov-registry:/usr/local/cargo/registry \
  -v orca-cov-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  "${IMG}" \
  bash -euo pipefail -c '
    rustup component add llvm-tools-preview >/dev/null 2>&1 || true
    if ! command -v cargo-nextest >/dev/null 2>&1; then
      case "$(uname -m)" in
        aarch64|arm64) NT_URL="https://get.nexte.st/latest/linux-arm" ;;
        *)             NT_URL="https://get.nexte.st/latest/linux" ;;
      esac
      curl -sSL "$NT_URL" | tar zxf - -C /usr/local/cargo/bin
    fi
    if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
      ARCH="$(uname -m)"; TRIPLE="${ARCH}-unknown-linux-gnu"
      curl -sSL "https://github.com/taiki-e/cargo-llvm-cov/releases/latest/download/cargo-llvm-cov-${TRIPLE}.tar.gz" | tar zxf - -C /usr/local/cargo/bin
    fi
    cargo llvm-cov nextest --no-report --locked --workspace --no-fail-fast
    cargo llvm-cov report
  '
