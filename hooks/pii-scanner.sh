#!/usr/bin/env bash
# PostToolUse hook — scans written/edited files for PII before it can leak.
# Exit 2 = block + warn the model. Exit 0 = clean.
# Output goes to stderr so the Claude Code runner displays the blocking message.

set -euo pipefail

INPUT=$(cat)

FILE_PATH=$(echo "$INPUT" | python3 -c "
import sys, json
data = json.load(sys.stdin)
inp = data.get('tool_input', {})
# Write uses file_path; Edit uses file_path
print(inp.get('file_path', ''))
" 2>/dev/null || true)

[[ -z "$FILE_PATH" ]] && exit 0
[[ ! -f "$FILE_PATH" ]] && exit 0

# ── Exclusions — private internal files, not public-facing content ────────────
# Brain vault agent definitions and commands document PII patterns as examples;
# they are private and never published.
case "$FILE_PATH" in
  "$HOME/orca/ai/claude/agents/"*) exit 0 ;;
  "$HOME/orca/ai/claude/commands/"*) exit 0 ;;
  "$HOME/dotfiles/obsidian/ai/claude/agents/"*) exit 0 ;;
  "$HOME/dotfiles/obsidian/ai/claude/commands/"*) exit 0 ;;
  "$HOME/.claude/CLAUDE.md") exit 0 ;;
esac

# ── PII patterns ──────────────────────────────────────────────────────────────
PATTERNS=(
  # Phone numbers (US, with common formatting variants)
  '\+?1[-.\s]?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}'
  # Scott's area code + specific prefix (belt-and-suspenders)
  '435[-.\s]?590'
  # Known personal Gmail alias
  'scottdkey\+contact'
  # Gmail personal address
  'scott\.key@gmail\.com'
  # SSN shape
  '\b\d{3}-\d{2}-\d{4}\b'
  # Staging domain — must not appear in public files
  'yuber\.app'
  # Raw API key prefixes (Resend, Stripe, generic Bearer)
  're_[A-Za-z0-9]{20,}'
  'sk_live_[A-Za-z0-9]+'
  'pk_live_[A-Za-z0-9]+'
  'Bearer [A-Za-z0-9\-_\.]{20,}'
  # Cloudflare Turnstile secret (starts with 0x)
  '0x[A-Fa-f0-9]{32,}'
)

FINDINGS=""
for PATTERN in "${PATTERNS[@]}"; do
  MATCH=$(grep -nEo "$PATTERN" "$FILE_PATH" 2>/dev/null | head -5 || true)
  if [[ -n "$MATCH" ]]; then
    FINDINGS+="  [PATTERN: $PATTERN]\n$MATCH\n\n"
  fi
done

if [[ -n "$FINDINGS" ]]; then
  {
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "⚠  PII SCANNER — POTENTIAL SENSITIVE DATA DETECTED"
    echo "   File: $FILE_PATH"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo -e "$FINDINGS"
    echo "Review before committing. Remove or move to GH Actions secret."
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  } >&2
  exit 2
fi

exit 0
