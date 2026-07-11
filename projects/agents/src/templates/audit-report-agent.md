---
name: audit-report-agent
description: Template for audit agents that produce structured sweep reports. Copy this structure and replace placeholders with agent-specific content.
---

# Audit Agent Template

Use this template when building an agent that sweeps a target and produces a structured findings report (security, privacy, test coverage, infra, contracts, etc.).

## Frontmatter

```yaml
---
name: <agentname>
description: <one-line role description>
tools: Read, Glob, Grep, Bash, Agent, TodoWrite, TodoRead
model: inherit
color: <pick one>
---
```

## Persona

One paragraph. State the agent's name, its job, and what it does NOT do (to prevent scope creep). No filler.

## What you check

Domain-specific checklist. Group by category. Use bullet points.

## Workflow

Follows the `/survey-confirm-fix` workflow. <AgentName>-specific extensions:

### Phase 1 — Survey
[What the agent reads, runs, or maps to build its picture. Domain-specific.]

### Phase 2 — Build todo list
Prioritized per `~/.orca/SEVERITY_RUBRIC.md`. Each item: what it is, where (file:line or component), what the fix is.

### Phase 3 — Report or fix
[Report-only agents: describe report format. Fix agents: describe confirm-and-fix loop.]

## Report format

```
<AGENTNAME> <DOMAIN> AUDIT
Target: <path or scope>
[Relevant metadata: framework, counts, date]

━━━ <CATEGORY NAME> (N) ━━━

[1] <finding title>: <file or location>
    <one-line problem description>
    Action: <concrete remediation>

━━━ <NEXT CATEGORY> (N) ━━━

[1] ...

━━━ PASSING (N items with no issues) ━━━
```

**Format rules:**
- Section dividers: `━━━ CATEGORY NAME (N) ━━━`
- Findings: numbered `[1]`, `[2]` within section, never globally
- Each finding: title/location on first line, detail indented, Action: line
- Always end with a PASSING section, even if empty
- Severity order: CRITICAL → HIGH → MEDIUM → LOW → PASSING

## Delegation

Consult the relevant KB agent for project-specific context before asserting findings.

See `~/.orca/DELEGATION.md` for the full routing table.

## Rules

- Never modify anything without explicit permission. Report by default.
- Every finding must cite file + line (or component name). No vague findings.
- See `~/.orca/SEVERITY_RUBRIC.md` for severity definitions.
- See `~/.orca/TOOL_RULES.md` for modification policy.

## Compliance checklist

Before publishing an agent built on this template, verify every item:

- [ ] `tools` frontmatter includes `TodoWrite, TodoRead`
- [ ] `tools` frontmatter does NOT include `Write` or `Edit` (audit agents are read-only)
- [ ] Workflow references `/survey-confirm-fix` skill
- [ ] Phase 2 references `~/.orca/SEVERITY_RUBRIC.md` — no inline level definitions
- [ ] Report format section cites `~/.orca/agent-templates/audit-report-agent.md`
- [ ] Rules references `~/.orca/TOOL_RULES.md` (read-only section)
- [ ] Delegation references `~/.orca/DELEGATION.md` — no inline routing tables
- [ ] Agent added to wolf.md routing table
- [ ] Agent added to `~/.orca/DELEGATION.md` specialist table
