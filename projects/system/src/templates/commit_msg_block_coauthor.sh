#!/usr/bin/env bash
# Global git commit-msg guard -- materialized by `orca install` to
# ~/.config/git/hooks/commit-msg and activated via
# `git config --global core.hooksPath`.
#
# Hard rule from the operator: an AI / assistant attribution trailer -- OR any
# "Generated with Claude Code" credit, 🤖 glyph, or attribution link --
# must NEVER appear in any commit on this machine. This rejects it at the git
# layer for EVERY repo, regardless of what created the commit. The Claude
# PreToolUse guard (~/.claude/hooks/block-coauthor.sh) is the first line for
# tool-driven commits and PR bodies; this is the backstop. Do not weaken it.
set -euo pipefail

msg_file="$1"

patterns=(
  'co[-_ ]?authored?[- _]?by'
  'generated[[:space:]]+with.*claude'
  'claude[[:space:]-]?code'
  'claude\.(com|ai)'
  'anthropic\.com'
  '🤖'
)

for pat in "${patterns[@]}"; do
  if grep -qiE "$pat" "$msg_file"; then
    echo "BLOCKED (global commit-msg hook): commit message contains forbidden" >&2
    echo "assistant attribution (matched /$pat/). Remove it and commit again." >&2
    exit 1
  fi
done

# Don't shadow a repo-local commit-msg hook: chain to it if present. The repo's
# real hooks live under its git dir, independent of the global core.hooksPath.
git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
local_hook="${git_dir:+$git_dir/hooks/commit-msg}"
if [[ -n "$local_hook" && -x "$local_hook" && "$local_hook" != "$0" ]]; then
  exec "$local_hook" "$@"
fi

exit 0
