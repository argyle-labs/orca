# Lint Agent Template

Use this template when building a lint/type-check agent for a specific project. The workflow is standard — only the project context differs.

Agents built from this template invoke `/lint-workflow` skill for the standard process. They add only what is project-specific: the tool commands, key rules, and common fix patterns.

---

## Frontmatter

```yaml
---
name: <project>-lint
description: <Project> linting agent. Runs ESLint (and optionally Prettier/TypeScript) against source files, surfaces errors with file+line context, and suggests fixes. Use before committing or after any edit.
tools: Bash, Read, Glob, Grep
model: inherit
---
```

**Tools:** Lint agents run commands and read files. They do not write/edit (fixes are proposed, not applied) unless explicitly asked.

---

## Body structure

### 1. Identity (1–2 lines)
What project this lints. What tools it runs.

### 2. Project context
The minimum context to run lint correctly:

```markdown
## Project context

- Monorepo root / working directory: `<path>`
- Package manager: `<pnpm | npm | yarn>`
- Lint command: `<command>`
- Type-check command: `<command>` (if applicable)
- Config files: `.eslintrc.*`, `tsconfig.json`, etc.
```

### 3. Workflow
Reference the `/lint-workflow` skill. Add only project-specific overrides:

```markdown
## Workflow

Follow the standard `/lint-workflow`. Project-specific overrides:

- If pnpm is not on PATH, fall back to: `node_modules/.bin/eslint . --no-fix`
- Lint runs from: `<directory>`
- Type-check uses: `<tsc --noEmit | pnpm typecheck>`
```

### 4. Key rules (project-specific)
The rules that actually fire in this project. Do NOT list rules that don't apply — that creates noise.

```markdown
## Key rules in play

- `rule-name` — what it catches, what the correct fix is
- `rule-name` — ...
```

### 5. Common fix patterns
The 3–6 patterns that appear most often with their correct solutions:

```markdown
## Common patterns and fixes

- **`any` type** → find the correct type in `<canonical source>` (see `CANONICAL_SOURCES.md`)
- **`console.log`** → replace with `logger.<level>()` from `<import>`
- **Pattern X** → correct fix
```

### 6. Do not auto-fix
Reference `TOOL_RULES.md` modification policy — propose fixes, wait for confirmation.

---

## What NOT to include

- ❌ The full lint workflow — it lives in `/lint-workflow` skill
- ❌ Tool guardrails — see `TOOL_RULES.md`
- ❌ Rules that don't apply to this project
- ❌ How to install lint tools (assume they're installed)
