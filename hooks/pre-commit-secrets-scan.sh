#!/usr/bin/env bash
# Blocks git commit if credentials or secrets are detected in staged changes.
# Uses gitleaks if installed; falls back to grep-based pattern matching.

set -uo pipefail

CMD=$(jq -r '.tool_input.command // ""' 2>/dev/null)

# Only act on git commit commands
if ! echo "$CMD" | grep -qE 'git\s+commit'; then
  exit 0
fi

SCAN_RESULT=0
FINDINGS=""

if command -v gitleaks &>/dev/null; then
  OUTPUT=$(gitleaks protect --staged -v 2>&1) || SCAN_RESULT=$?
  if [ "$SCAN_RESULT" -ne 0 ]; then
    FINDINGS=$(echo "$OUTPUT" | grep -E 'RuleID|Secret|File|Line' | head -20 | tr '\n' '|' | sed 's/|/ — /g')
  fi
else
  # Fallback: scan staged diff for common secret patterns
  DIFF=$(git diff --cached 2>/dev/null) || exit 0  # not a git repo, pass through

  declare -A PATTERN_LABELS
  PATTERN_LABELS["eyJ[A-Za-z0-9_-]{20,}\\.[A-Za-z0-9_-]{20,}\\.[A-Za-z0-9_-]{20,}"]="JWT token"
  PATTERN_LABELS["[Aa]uthorization['\": ]+[Bb]earer [A-Za-z0-9_.\\-]{20,}"]="Bearer token"
  PATTERN_LABELS["['\"-]password['\": ]+[A-Za-z0-9!@#$%^&*_.\\-]{8,}"]="Password value"
  PATTERN_LABELS["[Aa][Pp][Ii][_-]?[Kk][Ee][Yy]['\": =]+[A-Za-z0-9_.\\-]{16,}"]="API key"
  PATTERN_LABELS["[Ss][Ee][Cc][Rr][Ee][Tt]['\": =]+[A-Za-z0-9_.\\-]{8,}"]="Secret value"
  PATTERN_LABELS["-----BEGIN (RSA|EC|OPENSSH|PGP) PRIVATE KEY"]="Private key"
  PATTERN_LABELS["[Aa][Ww][Ss]_[Aa][Cc][Cc][Ee][Ss][Ss][_-]?[Kk][Ee][Yy][_-]?[Ii][Dd]"]="AWS access key"

  for pattern in "${!PATTERN_LABELS[@]}"; do
    if echo "$DIFF" | grep -qE "^\+.*($pattern)"; then
      FINDINGS+="${PATTERN_LABELS[$pattern]}, "
      SCAN_RESULT=1
    fi
  done

  if [ "$SCAN_RESULT" -ne 0 ]; then
    FINDINGS="Detected: ${FINDINGS%, } (install gitleaks for comprehensive scanning: brew install gitleaks)"
  fi
fi

if [ "$SCAN_RESULT" -ne 0 ]; then
  jq -n \
    --arg reason "Secrets scan blocked this commit. $FINDINGS Remove credentials before committing." \
    '{"continue": false, "stopReason": $reason}'
  exit 0
fi

exit 0
