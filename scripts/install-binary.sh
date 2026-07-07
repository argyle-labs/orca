#!/usr/bin/env bash
# Install a built orca binary to a destination path. Thin wrapper over
# scripts/release-lib.sh's install_orca_binary — single source of truth so
# `make deploy`, `make install-dev`, `make watch`, GitHub Actions, and Gitea
# Actions all behave identically (symlink stripping, idempotent copy, macOS
# codesign).
#
# Usage:
#   scripts/install-binary.sh <source-binary> [destination]
#
# Defaults:
#   destination → $HOME/.local/bin/orca
#
# Examples:
#   scripts/install-binary.sh target/aarch64-apple-darwin/release/orca
#   scripts/install-binary.sh target/debug/orca ~/.local/bin/orca

set -euo pipefail

# shellcheck source=./release-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/release-lib.sh"

src="${1:-}"
dest="${2:-$HOME/.local/bin/orca}"

[ -n "$src" ] || { echo "usage: install-binary.sh <source-binary> [destination]" >&2; exit 2; }

install_orca_binary "$src" "$dest"
