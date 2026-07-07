# New Agent Bootstrap

Bootstraps a new agent from the correct template. Prevents drift by generating a pre-wired file, verifying its compliance checklist, and registering it in wolf.md and DELEGATION.md before the agent is ever used.

---

## Step 1 — Classify the agent

Ask the user: **"What does this agent do?"**

Map the answer to the correct template class:

| If the agent... | Template |
|-----------------|----------|
| Sweeps a target and produces a structured report — no modifications | `audit-report-agent.md` |
| Reviews a domain, confirms each fix one at a time before applying | `confirm-fix-agent.md` |
| Reads sources and produces authoritative explanations — never modifies | `read-explain-agent.md` |
| Answers questions about a codebase or technology — no modifications | `kb-agent.md` |
| Runs a linter and surfaces findings | `lint-agent.md` |
| Runs the TypeScript compiler and walks through errors | `typecheck-agent.md` |
| Formats, posts, or validates PR review comments | `pr-review-agent.md` |
| Authors, tests, and deploys database migrations | `migration-agent.md` |
| Routes tasks to other agents | `orchestrator-agent.md` |

If the agent fits multiple classes, prefer the most specific. If uncertain, ask one clarifying question.

## Step 2 — Collect the essentials

Ask the user (or infer from context):

1. **Name** — `lowercase-hyphenated`, max 15 chars, memorable
2. **Description** — one sentence, starts with a verb; this is what appears in wolf.md
3. **Color** — pick from: `red`, `orange`, `yellow`, `green`, `blue`, `purple`, `pink`, `cyan`
4. **Scope** — what does this agent operate on? (file paths, repos, services)

## Step 3 — Read the template

Read `~/brain/ai/claude/agent-templates/<template-name>.md`.

Extract:
- Required `tools` list from the frontmatter block
- Compliance checklist items (the `[ ]` list at the bottom)
- Required canonical doc references (SEVERITY_RUBRIC, DELEGATION, TOOL_RULES, etc.)

## Step 4 — Generate the agent file

Write to `~/brain/ai/claude/agents/<name>.md`.

Structure:

```markdown
---
name: <name>
description: <one-line description>
tools: <required tools from template>
model: inherit
color: <color>
---

You are <Name> — <one-line character/role statement>.

<Persona paragraph: what the agent does, what it does NOT do, what happens when it finds something outside its domain.>

## What you check

<Domain-specific checklist, grouped by category.>

## Workflow

Follows the `/survey-confirm-fix` workflow. <Name>-specific extensions:

### Phase 1 — Survey
<What to read, run, or map. Domain-specific.>

### Phase 2 — Build todo list
Prioritized per `~/brain/ai/claude/SEVERITY_RUBRIC.md`. Each item: what it is, where (file:line), what the fix is.

### Phase 3 — Confirm and fix, one at a time
<confirm-fix agents: the [y/n/skip] loop>
<audit agents: report format>

### Phase 4 — Summary
Fixed / Skipped / Remaining.

## Delegation

<domain-specific routing context if needed>

See `~/brain/ai/claude/DELEGATION.md` for the full routing table.

## Rules

- <domain-specific rule 1>
- <domain-specific rule 2>
- See `~/brain/ai/claude/TOOL_RULES.md` for the modification and verify-after policy.
```

**Fill in all `<placeholders>` with real content. Do not leave template boilerplate in the output.**

For agents that run shell commands, use `$HOME` in all bash paths — never a hardcoded `/Users/<name>/` or `/home/<name>/`.

## Step 5 — Verify the compliance checklist

Read the template's compliance checklist. For each item, verify the generated file satisfies it.

Present the results:

```
Compliance check: <name>.md
━━━ PASSING ━━━
✓ tools frontmatter includes TodoWrite, TodoRead
✓ Workflow references /survey-confirm-fix skill
...

━━━ FAILING ━━━
✗ Agent not yet in wolf.md routing table  → fixed in Step 6
✗ Agent not yet in DELEGATION.md          → fixed in Step 6
```

Items marked "fixed in Step 6" are expected — they get resolved next.

## Step 6 — Register in wolf.md

Add a row to the routing table in `~/brain/ai/claude/agents/wolf.md`:

```
| **@<name>** | <trigger description — when does Brain route here?> |
```

Place it near agents with similar roles.

## Step 7 — Register in DELEGATION.md

Add a row to the specialist table in `~/brain/ai/claude/DELEGATION.md`:

```
| **@<name>** | <one-line role — same as wolf.md trigger> |
```

Place it in the "General specialists" section unless it belongs to a project KB group.

## Step 8 — Final verification

Tell the user:

```
Agent <name>.md created and registered.
Run @wren to verify consistency across all agents.
```

---

## Template quick-pick

When the user gives a short description, this shortcut applies:

- "reviews and fixes" → `confirm-fix-agent.md`
- "sweeps and reports" → `audit-report-agent.md`
- "reads and explains" → `read-explain-agent.md`
- "codebase knowledge" → `kb-agent.md`
- "linting" → `lint-agent.md`
- "type checking" → `typecheck-agent.md`
- "PR comments" → `pr-review-agent.md`
- "migrations" → `migration-agent.md`
- "orchestrates other agents" → `orchestrator-agent.md`
