#!/usr/bin/env bash
# orca-managed: Claude attribution guard.
#
# Materialized by `orca install` to ~/.claude/hooks/block-coauthor.sh and wired
# as a PreToolUse:Bash|Write|Edit hook in ~/.claude/settings.json. Blocks any
# tool call whose payload carries assistant self-attribution in ANY form --
# trailers, "Generated with Claude Code" credit lines, the 🤖 signature
# glyph, or attribution links -- in commits, PR bodies (`gh pr create/edit`),
# and files alike. Hard rule from the operator: this attribution must NEVER
# reach a commit, PR, or file. Do not remove or weaken it.
# Exit 2 = blocking error; stderr is shown to the assistant.
set -euo pipefail

payload=$(cat)

# Bash: tool_input.command (covers `git commit`, `gh pr create/edit`, heredocs).
# Write/Edit: tool_input.content, .new_string, .old_string.
haystack=$(printf '%s' "$payload" | jq -r '
  [
    .tool_input.command // empty,
    .tool_input.content // empty,
    .tool_input.new_string // empty,
    .tool_input.old_string // empty
  ] | join("\n")
')

# Each pattern is a distinct attribution form. Case-insensitive.
patterns=(
  'co[-_ ]?authored?[- _]?by'
  'co[-_ ]?authored?'
  'generated[[:space:]]+with.*claude'
  'claude[[:space:]-]?code'
  'claude\.(com|ai)'
  'anthropic\.com'
  '🤖'
)

for pat in "${patterns[@]}"; do
  if printf '%s' "$haystack" | grep -Eiq "$pat"; then
    echo "Blocked: payload contains assistant attribution (matched /$pat/). The operator forbids ALL such attribution -- no trailers, no 'Generated with Claude Code' credits, no 🤖 emoji, no attribution links. Remove it entirely and retry." >&2
    exit 2
  fi
done

exit 0
