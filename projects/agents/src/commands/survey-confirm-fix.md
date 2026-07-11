# Survey → Confirm → Fix Workflow

Standard 4-phase workflow for agents that review, audit, or clean up a target. Used by bear, ferret, ibis, swift, magpie, jackdaw, and any other agent that finds-then-fixes.

---

## Phase 1 — Survey (silent)

Read everything relevant. Do **not** report findings as you go — collect all issues first.

What to collect:
- The specific problem (not vague — "line 42 does X which breaks Y" not "this looks wrong")
- Where it is (file + line number or component name)
- What the fix is (concrete action, not a description of the action)
- Severity: Critical / Major / Minor

Do not stop early. Complete the full survey before moving to Phase 2.

## Phase 2 — Build todo list

Write all findings to `TodoWrite` as a prioritized list:

- **Critical** items first (broken, security risk, data loss)
- **Major** next (gaps, wrong behavior, missing coverage)
- **Minor** last (stale refs, naming, style)

Each todo item must state:
1. What the problem is (specific)
2. Where it is (file:line or component)
3. What the fix is (action, not description)

## Phase 3 — Confirm and resolve, one by one

Present the first item:

```
[1/N] CRITICAL — path/to/file.ts:42
Problem: <specific issue>
Fix: <specific action>
Proceed? [y/n/skip]
```

Responses:
- **y** → apply the fix immediately, verify it worked, mark done, move to next
- **n** → stop, ask what the user wants instead before continuing
- **skip** → mark skipped with a note, move to next

**Never batch multiple fixes into one confirmation. One item, one confirmation.**

After each fix: re-read the affected file or run a quick check to verify the change is correct.

## Phase 4 — Summary

When the list is exhausted:

```
Done.
✓ Fixed: N
⤼ Skipped: N (reasons)
○ Remaining: N (if user stopped early)
```

---

## Scope note

This workflow applies within the current task's scope. If auditing a repo, scope = that repo. If auditing brain agents, scope = brain agents. Do not cross scope boundaries without explicit user instruction.
