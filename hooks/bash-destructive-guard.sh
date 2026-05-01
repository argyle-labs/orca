#!/bin/bash
# bash-destructive-guard.sh — block destructive shell commands against homelab infrastructure
# Fires on PreToolUse:Bash. Reads tool input JSON from stdin.
# Exit 2 = block the command and surface the message to Claude.

input=$(cat)
command=$(echo "$input" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('command',''))" 2>/dev/null)

if [[ -z "$command" ]]; then
  exit 0
fi

PATTERNS=(
  "rm -rf"
  "rm -fr"
  "qm destroy"
  "pct destroy"
  "pvesm remove"
  "wipefs"
  "mkfs\."
  "dd if="
  "blkdiscard"
  "shred "
  "format "
)

for pattern in "${PATTERNS[@]}"; do
  if echo "$command" | grep -qE "$pattern"; then
    echo "BLOCKED: Destructive command detected: '$pattern'"
    echo "Command: $command"
    echo ""
    echo "This command requires explicit user confirmation before running."
    echo "State what you intend to do and why, then ask the user to approve."
    exit 2
  fi
done

exit 0
