#!/usr/bin/env bash
# Global git commit-msg guard — materialized by `orca install` to
# ~/.config/git/hooks/commit-msg and activated via
# `git config --global core.hooksPath ~/.config/git/hooks`.
#
# Hard rule from the operator: an AI / assistant attribution trailer must NEVER
# appear in any commit on this machine. This rejects it at the git layer for
# EVERY repo, regardless of what created the commit. Do not remove or weaken it.
set -euo pipefail

msg_file="$1"

if grep -qiE 'co[-_ ]?authored?[-_ ]?by:[[:space:]]*(claude|anthropic|[^[:space:]]*noreply@anthropic)' "$msg_file"; then
  echo "BLOCKED (global commit-msg hook): commit message contains a forbidden" >&2
  echo "AI attribution trailer. Remove that trailer line and commit again." >&2
  exit 1
fi

# Don't shadow a repo-local commit-msg hook: chain to it if present. The repo's
# real hooks live under its git dir, independent of the global core.hooksPath.
git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
local_hook="${git_dir:+$git_dir/hooks/commit-msg}"
if [[ -n "$local_hook" && -x "$local_hook" && "$local_hook" != "$0" ]]; then
  exec "$local_hook" "$@"
fi

exit 0
