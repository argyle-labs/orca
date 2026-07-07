#!/bin/bash
# opnsense-guard.sh — intercept any Bash command targeting OPNsense before execution
# Fires on PreToolUse:Bash. Reads tool input JSON from stdin.
# Exit 2 = block and surface message to Claude.

input=$(cat)
command=$(echo "$input" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('command',''))" 2>/dev/null)

if [[ -z "$command" ]]; then
  exit 0
fi

# Named-host patterns are safe to ship. The router's IP is deployment-private,
# so it is NOT hardcoded: set ORCA_ROUTER_GUARD_IP to also block commands that
# target the router by address. When unset, only the named patterns apply.
OPNSENSE_PATTERNS=(
  "ssh.*opnsense"
  "opnsense-update"
  "curl.*opnsense"
  "wget.*opnsense"
)
if [[ -n "${ORCA_ROUTER_GUARD_IP:-}" ]]; then
  esc_ip=${ORCA_ROUTER_GUARD_IP//./\\.}
  OPNSENSE_PATTERNS+=("${esc_ip}([^0-9]|\$)")
fi

for pattern in "${OPNSENSE_PATTERNS[@]}"; do
  if echo "$command" | grep -qE "$pattern"; then
    echo "OPNSENSE GUARD: Command targets the OPNsense network router."
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
