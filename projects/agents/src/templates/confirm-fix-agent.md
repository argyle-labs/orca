---
name: confirm-fix-agent
description: Template for agents that survey a domain, build a prioritized todo list, and resolve issues one at a time with user confirmation. Use for code standards, doc consistency, placement audits, scope graduation, and accessibility reviews.
---

# Confirm-Fix Agent Template

Use this template when building an agent that: (1) reads silently, (2) produces a prioritized todo list, (3) confirms each fix with the user before applying. These agents CAN modify files — with confirmation, one item at a time.

Distinguished from audit-report agents (which are read-only and report only).

## Frontmatter

```yaml
---
name: <agentname>
description: <one-line role description>
tools: Read, Glob, Grep, Write, Edit, Bash, Agent, TodoWrite, TodoRead
model: inherit
color: <pick one>
---
```

## Persona

One paragraph. State: the agent's name, its job (what domain it enforces), and what it does NOT do (to prevent scope creep). End with: what happens if it finds an issue in another agent's territory — flag it, name the right agent, move on.

## What you check

Domain-specific checklist. Group by category. Use bullet points. This is the survey scope.

## Workflow

Follows the `/survey-confirm-fix` workflow. <AgentName>-specific extensions:

### Phase 1 — Survey
[What the agent reads, runs, or maps. Domain-specific. Collect all issues silently — do not report during this phase.]

### Phase 2 — Build todo list
Write to TodoWrite. Prioritized per `~/.orca/SEVERITY_RUBRIC.md`. Each item: what it is, where (file:line), what the fix is.

### Phase 3 — Confirm and fix, one at a time
```
[1/N] HIGH — <file>:<line>: <problem summary>
Fix: <concrete action>
Proceed? [y/n/skip]
```
- **y** — apply fix immediately, verify, mark done, move to next
- **n** — ask what the user wants instead
- **skip** — note reason, continue

After each fix, re-read the file to verify the change is correct before moving on.

### Phase 4 — Summary
Fixed / Skipped / Remaining.

## Delegation

Consult the relevant KB agent for project-specific context before flagging something as wrong — it may be an intentional convention.

See `~/.orca/DELEGATION.md` for the full routing table.

## Rules

- Read before criticizing — base every finding on what actually exists.
- Never batch fixes. One confirmation per change.
- If a finding is outside your domain, name the right agent and skip it.
- See `~/.orca/SEVERITY_RUBRIC.md` for severity definitions.
- See `~/.orca/TOOL_RULES.md` for the standard modification policy.

## Compliance checklist

Before publishing an agent built on this template, verify every item:

- [ ] `tools` frontmatter includes `TodoWrite, TodoRead, Write, Edit`
- [ ] Workflow references `/survey-confirm-fix` skill
- [ ] Phase 2 references `~/.orca/SEVERITY_RUBRIC.md` — no inline level definitions
- [ ] Phase 3 uses the `[y/n/skip]` confirm loop (not a custom loop)
- [ ] Rules references `~/.orca/TOOL_RULES.md` modification policy
- [ ] Delegation references `~/.orca/DELEGATION.md` — no inline routing tables
- [ ] Agent added to wolf.md routing table
- [ ] Agent added to `~/.orca/DELEGATION.md` specialist table
