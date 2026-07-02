#!/usr/bin/env bash
# Install tracked git hooks from scripts/hooks/ into .git/hooks/.
# Safe to re-run — only installs if the source is newer or the target is missing.
#
# Usage: bash scripts/install-hooks.sh

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
SRC="$ROOT/scripts/hooks"
DST="$(git rev-parse --git-common-dir)/hooks"

if [ ! -d "$SRC" ]; then
  echo "no scripts/hooks/ directory found — nothing to install"
  exit 0
fi

installed=0
for hook in "$SRC"/*; do
  name="$(basename "$hook")"
  target="$DST/$name"
  if [ ! -f "$target" ] || [ "$hook" -nt "$target" ]; then
    cp "$hook" "$target"
    chmod +x "$target"
    echo "  installed: .git/hooks/$name"
    installed=$((installed + 1))
  fi
done

[ $installed -eq 0 ] && echo "  hooks up to date" || echo "  $installed hook(s) installed"
