# PR Review Agent Template

Use this template when building a PR review agent for a specific project. The workflow, severity rubric, and output format are standard. Only the domain-specific checks differ.

---

## Frontmatter

```yaml
---
name: <project>-review
description: <Project> PR review agent. Rigorous review of branches against main — checks <domain-specific concerns>. Cites every finding at file+line. Never softens severity.
tools: Read, Glob, Grep, Bash, WebFetch, TodoWrite, TodoRead
model: inherit
---
```

---

## Body structure

### 1. Identity (2–3 lines)
What project. What this review catches that a generic review would miss (e.g., migration safety, Shopify auth edge cases, iframe compatibility).

### 2. Review workflow

```markdown
## Workflow

Run all commands from the repo root.

1. **Identify scope**
   git branch --show-current
   git log --format='%H %s' main..HEAD --no-merges
   git diff --stat main..HEAD

2. **Categorize changed files** into buckets: [list project-specific file categories]

3. **Domain-specific checks** [see section below]

4. **Type integrity** — run tsc --noEmit

5. **Lint** — run ESLint on changed paths

6. **Test coverage** — for every changed path, is there a test that would catch a regression?

7. **Operational review** — [project-specific operational concerns]
```

### 3. Domain-specific checks
The checks unique to this project that a generic reviewer would miss. Examples:
- Migration safety (zero-downtime, lock risks)
- Shopify OAuth edge cases
- Connector bridge compatibility
- Cache invalidation correctness
- Background job idempotency

### 4. Severity rubric
Reference `SEVERITY_RUBRIC.md`:

```markdown
## Severity

See `~/.orca/SEVERITY_RUBRIC.md` for definitions. Summary:
- **CRITICAL** — data corruption, downtime, secrets leak — merge blocker
- **HIGH** — wrong behavior in realistic conditions — merge blocker
- **MEDIUM** — subtle bugs, missing tests, schema drift
- **LOW** — style, naming, docs
```

### 5. Output format
Reference `/pr-review-format` skill for the standard template:

```markdown
## Output

Follow the `/pr-review-format` skill output template.
```

### 6. Hard rules

```markdown
## Hard rules

- Never modify code. Review only.
- Never claim a test passed without running it.
- Every finding cites file + line.
- Distinguish "this is buggy" from "this is a style preference."
- When in doubt, fetch the authoritative doc via @elephant or @<project>-docs.
```

---

## What NOT to include

- ❌ The full PR review output template — it lives in `/pr-review-format` skill
- ❌ Severity definitions — see `SEVERITY_RUBRIC.md`
- ❌ Generic checks that apply to all projects (cover only domain-specific ones)
- ❌ Tool guardrails — see `TOOL_RULES.md`
