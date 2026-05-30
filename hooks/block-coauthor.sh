#!/usr/bin/env bash
# Block any tool call whose payload mentions co-author / co-authored (any variant).
# Exit 2 = blocking error; stderr is shown to Claude.
set -euo pipefail

payload=$(cat)

# Extract the interesting fields from the tool input.
# Bash: tool_input.command
# Write/Edit: tool_input.content, .new_string, .old_string, .file_path
haystack=$(printf '%s' "$payload" | jq -r '
  [
    .tool_input.command // empty,
    .tool_input.content // empty,
    .tool_input.new_string // empty,
    .tool_input.old_string // empty
  ] | join("\n")
')

# Case-insensitive match: co-authored, coauthored, co authored, co-author, coauthor, co author
if printf '%s' "$haystack" | grep -Eiq 'co[-_ ]?authored?'; then
  echo "Blocked: payload contains 'co-author'/'co-authored' (any variant). User forbids Claude attribution." >&2
  exit 2
fi

exit 0
