#!/usr/bin/env bash
# Stop hook — reads the last assistant message from the Claude transcript
# and appends it to the orca session JSONL.
# Fires after every Claude response.

set -euo pipefail

INPUT=$(cat)

export HOOK_SESSION_ID=$(echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('session_id',''))" 2>/dev/null || true)
export HOOK_TRANSCRIPT=$(echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('transcript_path',''))" 2>/dev/null || true)
export HOOK_CWD=$(echo "$INPUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('cwd',''))" 2>/dev/null || true)

[[ -z "$HOOK_SESSION_ID" || -z "$HOOK_TRANSCRIPT" ]] && exit 0
[[ ! -f "$HOOK_TRANSCRIPT" ]] && exit 0

export HOOK_PROJECT=$(basename "$HOOK_CWD" 2>/dev/null || echo "unknown")
export HOOK_DATE=$(date +%Y-%m-%d)
export HOOK_SHORT="${HOOK_SESSION_ID:0:8}"
export HOOK_LOG_DIR="$HOME/orca/ai/claude/logs/sessions"
mkdir -p "$HOOK_LOG_DIR"

python3 - <<'PYEOF'
import json, uuid, os
from datetime import datetime, timezone

session_short  = os.environ["HOOK_SHORT"]
project        = os.environ["HOOK_PROJECT"]
transcript_path = os.environ["HOOK_TRANSCRIPT"]
log_dir        = os.environ["HOOK_LOG_DIR"]
date           = os.environ["HOOK_DATE"]

session_file = f"{log_dir}/{date}_{session_short}_{project}.jsonl"

# Walk the transcript to find the last assistant message
last_assistant = None
try:
    with open(transcript_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if entry.get("type") == "assistant":
                msg = entry.get("message", {})
                if msg.get("role") == "assistant":
                    last_assistant = msg
except Exception:
    pass

if not last_assistant:
    raise SystemExit(0)

# Extract only text blocks (skip thinking, tool_use, tool_result)
text_parts = []
for block in last_assistant.get("content", []):
    if isinstance(block, dict) and block.get("type") == "text":
        text_parts.append(block.get("text", ""))

content = " ".join(text_parts).strip()
if not content:
    raise SystemExit(0)

# Trim to a reasonable length
content = content[:1200]

record = {
    "id": str(uuid.uuid4()),
    "session": session_short,
    "timestamp": datetime.now(timezone.utc).isoformat(),
    "project": project,
    "role": "assistant",
    "agent": "orca",
    "content": content,
    "important": False,
    "tags": [],
    "note": ""
}

with open(session_file, "a") as f:
    f.write(json.dumps(record) + "\n")
PYEOF

exit 0
