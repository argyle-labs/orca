# Orchestrator Agent Template

Use this template when building an agent whose primary job is **routing and synthesis** — not direct execution.

Orchestrators identify what is needed, delegate to the right specialist, and synthesize the results. They do not duplicate specialist logic. They do not embed knowledge that a skill or KB agent already owns.

---

## Frontmatter

```yaml
---
name: <kebab-case-name>
description: <One sentence: what this orchestrator routes, and when to use it. Be specific about the domain.>
tools: Read, Glob, Grep, Bash, Agent
model: inherit
---
```

**Tools:** Orchestrators need Read/Glob/Grep/Bash for lightweight context gathering. Agent for delegation. They do NOT need Write/Edit unless they also produce output artifacts.

---

## Body structure

### 1. Identity (2–4 lines)
Who this orchestrator is. What domain it owns. What it does NOT do (what it routes to others).

### 2. Routing table
A table or list mapping task types → agents or skills. Be specific. "Ask @fox for root cause" is better than "delegate debugging."

```markdown
| Task type | Route to |
|-----------|----------|
| Understand the codebase | @<kb-agent> |
| Find a bug | @fox |
| Write code | @crow |
| Review security | @viper |
| Load project context | /<context-skill> |
```

### 3. Context-loading pattern
How the orchestrator gathers just enough context to route correctly, without loading everything:

```markdown
1. Read the user's request — identify the target project/domain
2. Load project context via the appropriate skill (/<project>-context)
3. Identify the right specialist from the routing table
4. Delegate with a complete brief — include what context was loaded, what the user asked, what the specialist should return
```

### 4. Synthesis rules
What the orchestrator does with results before returning them to the user:
- Summarize, do not parrot raw agent output
- Cite specific files/lines from specialist findings
- Surface the most important finding first

### 5. Hard rules
- Never embed knowledge that a KB agent or context skill already owns. Call the skill instead.
- Never guess at codebase conventions. Load context first.
- If the right specialist is unclear, say so and ask the user one focused question.

---

## What NOT to include in an orchestrator

- ❌ Detailed workflow steps that belong in a specialist agent
- ❌ Project-specific patterns that belong in a context skill
- ❌ Severity rubrics, tool guardrails, or delegation tables — reference shared docs instead
- ❌ More than one level of routing logic (orchestrators route to specialists; specialists execute)
