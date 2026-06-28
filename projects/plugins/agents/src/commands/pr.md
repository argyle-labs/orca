# PR Review Workflow

Multi-phase PR review with state management and interactive approval.

## Arguments

`$ARGUMENTS`

Parse the first word as the subcommand:

- No arguments or `status` → Show current PR context
- `review <url>` → Fetch and review a PR
- `findings` → Display findings table
- `approve` → Interactive approval workflow
- `post` → Post approved comments to Bitbucket
- `list` → Show all PRs with saved state
- `switch <ref>` → Switch to a different PR
- `clear` → Reset current PR state
- `clear --all` → Reset all PR state

---

## State Management Script

All state operations use `~/.claude/scripts/pr-state.sh`. This keeps file operations deterministic.

```bash
# Quick reference - State management
# All commands accept --pr <repo/id> to address a specific PR directly.
# This bypasses the global current pointer, enabling concurrent sessions.
pr-state.sh [--pr <repo/id>] path                    # Get state file path
pr-state.sh [--pr <repo/id>] read                    # Output state JSON
pr-state.sh [--pr <repo/id>] info                    # Get PR info
pr-state.sh [--pr <repo/id>] counts                  # Get finding counts
pr-state.sh list                                      # List all states as JSON
pr-state.sh [--pr <repo/id>] get-pending             # Get pending findings
pr-state.sh [--pr <repo/id>] get-approved            # Get approved findings
pr-state.sh init <ws> <repo> <id>                    # Create/reset state + set current
pr-state.sh [--pr <repo/id>] update-pr               # Update PR metadata (stdin)
pr-state.sh [--pr <repo/id>] set-findings            # Replace all findings (stdin)
pr-state.sh [--pr <repo/id>] add-findings            # Append findings (stdin)
pr-state.sh [--pr <repo/id>] update-finding <id> <status>  # Update finding status
pr-state.sh [--pr <repo/id>] update-comment <id> <comment> # Update finding comment
pr-state.sh [--pr <repo/id>] mark-posted <id> <bb-id> [url] # Mark finding as posted
pr-state.sh switch <repo/pr>                          # Switch current pointer
pr-state.sh clear                                     # Clear current state
pr-state.sh clear-all                                 # Clear all states

# Quick reference - Posting comments (credentials handled internally)
pr-post-comment.sh inline <ws> <repo> <pr> <file> <line> <comment>
pr-post-comment.sh general <ws> <repo> <pr> <comment>
```

### Concurrent Review Support

After `init`, capture the PR reference (`<repo>/<prId>`) and pass `--pr <repo>/<prId>` on **every subsequent command** in the session. This ensures multiple sessions reviewing different PRs never conflict over the global current pointer.

---

## Subcommand: status (default)

If no arguments provided, show current state.

**Steps:**

```bash
~/.claude/scripts/pr-state.sh info      # or --pr <repo>/<prId> if known
~/.claude/scripts/pr-state.sh counts    # or --pr <repo>/<prId> if known
```

If `info` returns `null`, show:

```
No PR currently loaded.

Usage:
  /pr review <url>  - Fetch and review a PR
  /pr list          - Show all PRs with saved state
```

Otherwise display:

```
Current PR: #{prId} - {title}
Repository: {workspace}/{repo}
Author: {author}
Branch: {sourceBranch} → {targetBranch}

Findings: {total} total
  - {pending} pending
  - {approved} approved (ready to post)
  - {posted} posted
  - {rejected} rejected
  - {skipped} skipped

Commands:
  /pr findings  - View all findings
  /pr approve   - Review pending findings
  /pr post      - Post approved comments
  /pr list      - Show all PRs with state
  /pr clear     - Clear state
```

---

## Subcommand: review <url>

Fetch PR and generate structured findings.

### 1. Fetch PR context

```bash
~/.claude/scripts/fetch-pr-diff.sh <url_or_ref>
```

Parse the output to extract:

- `workspace`, `repo`, `pr_number` (for state init)
- `title`, `author`, `source_branch`, `target_branch`
- `diff_file` path
- Review context checklist

### 2. Initialize state and capture PR ref

```bash
~/.claude/scripts/pr-state.sh init <workspace> <repo> <prId>
```

**Important:** After init, store the PR ref as `<repo>/<prId>` (e.g., `admin-nextjs/2501`). Pass `--pr <repo>/<prId>` on all subsequent pr-state.sh commands in this session.

### 3. Update PR metadata

```bash
echo '{"url": "...", "title": "...", "author": "...", ...}' | ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> update-pr
```

### 4. Read and analyze the diff

Read the diff file. Review following the repo-specific checklist.

### 5. Generate findings

For each issue, create a finding object:

```json
{
  "id": "f1",
  "severity": "critical|warning|suggestion|note",
  "file": "path/to/file.ts",
  "line": 42,
  "title": "Short title (5-10 words)",
  "message": "Detailed explanation",
  "status": "pending",
  "suggestedComment": "Comment text for Bitbucket"
}
```

**Severity guidelines:**
| Severity | Use for |
|----------|---------|
| `critical` | Must fix before merge (security, data loss, broken functionality) |
| `warning` | Should address (performance, missing error handling) |
| `suggestion` | Nice to have (minor optimization, documentation) |
| `note` | Informational (good work acknowledgment, context) |

### 6. Save findings

```bash
echo '[{...}, {...}]' | ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> set-findings
```

Use `set-findings` (not `add-findings`) so re-reviews replace rather than append.

### 7. Display summary

```bash
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> counts
```

Output:

```
PR #{prId}: {title}
Author: {author}
Branch: {sourceBranch} → {targetBranch}
Scope: {files} files | +{additions} / -{deletions}

Generated {total} findings:
  - {critical} critical
  - {warning} warnings
  - {suggestion} suggestions
  - {note} notes

Run `/pr findings` to view, `/pr approve` to review for posting.
```

Also include:

- **Verdict:** APPROVED / APPROVED WITH SUGGESTIONS / CHANGES REQUESTED
- 1-2 sentence summary
- Key architectural notes if relevant

---

## Subcommand: findings

Display all findings in a structured table.

**Steps:**

```bash
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> info
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> read
```

If `info` returns `null`: `No PR loaded. Use /pr review <url> first.`

Otherwise parse the findings and display:

```
PR #{prId}: {title}

┌────┬──────────┬─────────────────────────────────────────┬──────┬──────────┐
│ ID │ Severity │ File                                    │ Line │ Status   │
├────┼──────────┼─────────────────────────────────────────┼──────┼──────────┤
│ f1 │ warning  │ path/to/file.ts                         │ 87   │ pending  │
│ f2 │ critical │ path/to/other.ts                        │ 45   │ approved │
└────┴──────────┴─────────────────────────────────────────┴──────┴──────────┘

{pending} pending, {approved} approved, {rejected} rejected

Use `/pr approve` to review pending findings.
```

Below the table, show finding details grouped by status.

---

## Subcommand: approve

Interactive approval workflow.

**Steps:**

```bash
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> get-pending
```

If empty: `All findings have been reviewed. Use /pr findings to see results.`

### For each pending finding:

Display:

```
Finding {n}/{total} [{severity}] {file}:{line}

**{title}**

{message}

Suggested comment:
> {suggestedComment}
```

Use **AskUserQuestion** with options:

| Option | Label   | Description                                  |
| ------ | ------- | -------------------------------------------- |
| 1      | Approve | Post this comment to the PR                  |
| 2      | Edit    | Modify the comment (uses "Other" text input) |
| 3      | Skip    | Keep for later                               |
| 4      | Reject  | Discard this finding                         |

**Handle responses:**

- **Approve:**

    ```bash
    ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> update-finding {id} approved
    ```

- **Edit:** User provides new text via "Other"

    ```bash
    ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> update-comment {id} "{new_comment}"
    ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> update-finding {id} approved
    ```

- **Skip:**

    ```bash
    ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> update-finding {id} skipped
    ```

- **Reject:**
    ```bash
    ~/.claude/scripts/pr-state.sh --pr <repo>/<prId> update-finding {id} rejected
    ```

After each: `[{n}/{total}] Finding {status}. {remaining} remaining.`

### When complete:

```bash
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> counts
```

```
Approval complete!

Results:
  - {approved} approved (ready to post)
  - {skipped} skipped (review later)
  - {rejected} rejected

Next: Use `/pr post` to post approved comments to Bitbucket.
```

---

## Subcommand: post

Post approved findings as inline comments to Bitbucket.

**IMPORTANT:** This uses `~/.claude/scripts/pr-post-comment.sh` which handles credentials internally. The LLM should NEVER read the credentials config file directly.

**Steps:**

### 1. Get approved findings and PR info

```bash
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> get-approved
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> info
```

If no approved findings: `No approved findings to post. Use /pr approve first.`

### 2. For each approved finding, post the comment

```bash
~/.claude/scripts/pr-post-comment.sh inline \
  <workspace> <repo> <pr-id> \
  "<file-path>" <line-number> \
  "<suggestedComment>"
```

The script returns JSON with the Bitbucket comment ID and URL:

```json
{
  "success": true,
  "id": 123456,
  "url": "https://bitbucket.org/.../comment-123456",
  "file": "path/to/file.ts",
  "line": 42
}
```

### 3. Mark finding as posted

```bash
~/.claude/scripts/pr-state.sh --pr <repo>/<prId> mark-posted <finding-id> <bb-comment-id> <url>
```

### 4. Display summary

```
Posted {count} comments to PR #{prId}:

  ✓ f1: src/components/Button.tsx:42
    → https://bitbucket.org/.../comment-123

  ✓ f3: src/utils/helpers.ts:100
    → https://bitbucket.org/.../comment-124

  ✗ f7: src/api/client.ts:55 (failed: rate limited)
```

If any posts fail, show the error and suggest retry.

---

## Subcommand: list

Show all PRs with saved state.

```bash
~/.claude/scripts/pr-state.sh list
```

Parse the JSON array and display:

```
Saved PR Reviews:

┌─────────────────────────────┬───────┬─────────┬──────────┬───────────┐
│ Repository                  │ PR #  │ Pending │ Approved │ Status    │
├─────────────────────────────┼───────┼─────────┼──────────┼───────────┤
│ example-extensions          │ 581   │ 7       │ 0        │ * current │
│ example-web                 │ 889   │ 0       │ 3        │           │
└─────────────────────────────┴───────┴─────────┴──────────┴───────────┘

Use `/pr switch <repo>/<pr>` to switch active PR.
```

---

## Subcommand: switch <ref>

Switch to a different PR.

```bash
~/.claude/scripts/pr-state.sh switch <ref>
```

Input formats: `onsite-js/889`, `onsite-js#889`, or full filename

On success: `Switched to PR #{prId}: {title}`

On error: `No saved state for {ref}. Use /pr review <url> to fetch it.`

---

## Subcommand: clear

Reset current PR state.

```bash
~/.claude/scripts/pr-state.sh clear
```

Output: `PR state cleared. Use /pr review <url> to start a new review.`

### Variant: clear --all

```bash
~/.claude/scripts/pr-state.sh clear-all
```

Output: `All PR state cleared ({count} reviews removed).`

---

## Review Principles

1. **Less is more** - Only flag material issues, not style preferences
2. **Be specific** - Include file paths, line numbers, and code examples
3. **Explain why** - Don't just say "fix this", explain the impact
4. **Suggest solutions** - Provide concrete alternatives
5. **Acknowledge good work** - Use `note` severity for positive callouts

## Comment Tone

All `suggestedComment` text MUST be **inquisitive, not directive**. Frame comments as questions and collaborative suggestions — never commands or demands.

**Good examples:**

- "Could this cause issues if `conditions` is empty? Should we add a guard here?"
- "Would it make sense to consolidate these three reduces into a single pass?"
- "This constructs a Tailwind class dynamically — does Tailwind's JIT pick these up, or would a static mapping be safer?"
- "Should these `console.log` calls be replaced with TODOs before merge?"

**Bad examples (never use these tones):**

- "Fix this." / "Remove this."
- "You should use X instead."
- "This is wrong."
- "Don't do this."

## Large PR Handling

If > 50 files:

1. Focus on high-impact files (APIs, services, data models)
2. Sample-check repetitive changes
3. Create a finding flagging PR size as a concern
4. Suggest splitting if logically separable

---

## Example Session

```bash
# Review a PR
/pr review https://bitbucket.org/my-workspace/example-web/pull-requests/889

# Check status
/pr status

# View findings
/pr findings

# Approve findings interactively
/pr approve

# Post approved comments to Bitbucket
/pr post

# List all saved PRs
/pr list

# Switch to different PR
/pr switch admin-nextjs/2414

# Clear current
/pr clear

# Clear all
/pr clear --all
```
