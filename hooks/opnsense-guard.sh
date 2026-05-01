#!/bin/bash
# opnsense-guard.sh — intercept any Bash command targeting OPNsense before execution
# Fires on PreToolUse:Bash. Reads tool input JSON from stdin.
# Exit 2 = block and surface message to Claude.

input=$(cat)
command=$(echo "$input" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('command',''))" 2>/dev/null)

if [[ -z "$command" ]]; then
  exit 0
fi

OPNSENSE_PATTERNS=(
  "10\.10\.10\.1[^0-9]"
  "10\.10\.10\.1$"
  "ssh.*opnsense"
  "opnsense-update"
  "curl.*opnsense"
  "wget.*opnsense"
)

for pattern in "${OPNSENSE_PATTERNS[@]}"; do
  if echo "$command" | grep -qE "$pattern"; then
    echo "OPNSENSE GUARD: Command targets OPNsense (10.10.10.1) — the network router."
    echo "Command: $command"
    echo ""
    echo "OPNsense protocol requires:"
    echo "  1. State exactly what you intend to change and why"
    echo "  2. Get explicit user confirmation before running"
    echo "  3. Make one change at a time, verify before the next step"
    echo ""
    echo "Do not proceed until the user has confirmed this specific command."
    exit 2
  fi
done

exit 0
