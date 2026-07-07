#!/usr/bin/env bash
# UserPromptSubmit hook — logs the user's prompt to the orca session JSONL.
# Fires every time the user submits a message.

set -euo pipefail

INPUT=$(cat)

export HOOK_SESSION_ID=$(echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('session_id',''))" 2>/dev/null || true)
export HOOK_CWD=$(echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('cwd',''))" 2>/dev/null || true)
export HOOK_PROMPT=$(echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('prompt','')[:800])" 2>/dev/null || true)

[[ -z "$HOOK_SESSION_ID" ]] && exit 0

export HOOK_PROJECT=$(basename "$HOOK_CWD" 2>/dev/null || echo "unknown")
export HOOK_DATE=$(date +%Y-%m-%d)
export HOOK_SHORT="${HOOK_SESSION_ID:0:8}"
export HOOK_LOG_DIR="$HOME/orca/ai/claude/logs/sessions"
mkdir -p "$HOOK_LOG_DIR"

python3 - <<'PYEOF'
import json, uuid, os
from datetime import datetime, timezone

session_short = os.environ["HOOK_SHORT"]
project       = os.environ["HOOK_PROJECT"]
prompt        = os.environ["HOOK_PROMPT"]
log_dir       = os.environ["HOOK_LOG_DIR"]
date          = os.environ["HOOK_DATE"]

session_file = f"{log_dir}/{date}_{session_short}_{project}.jsonl"

record = {
    "id": str(uuid.uuid4()),
    "session": session_short,
    "timestamp": datetime.now(timezone.utc).isoformat(),
    "project": project,
    "role": "user",
    "agent": None,
    "content": prompt,
    "important": False,
    "tags": [],
    "note": ""
}

with open(session_file, "a") as f:
    f.write(json.dumps(record) + "\n")
PYEOF

exit 0
