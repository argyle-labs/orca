# Lint Workflow

Standard lint workflow for all lint agents. The agent provides project-specific commands and rules; this skill provides the process.

---

## Workflow

### Step 1 — Scope the check

If the user named specific files or a feature area: lint only those paths.
For a full check: run the project's full lint command.

### Step 2 — Run lint

Use the project's lint command. If the primary tool is unavailable, fall back to `node_modules/.bin/eslint . --no-fix`.

Pipe output: `<lint-command> 2>&1 | head -300`

### Step 3 — Run type-check (if applicable)

If the project has a type-check command, run it separately.
Pipe output: `<typecheck-command> 2>&1 | head -200`

### Step 4 — Parse and group

Group findings by file. For each error or warning:
- State the **rule violated** (e.g., `@typescript-eslint/no-explicit-any`)
- Quote the **exact offending line**
- Give a **concrete fix** — code, not a description of what to do

### Step 5 — Prioritize

1. **Errors** (block build or CI) — must fix
2. **Warnings** — should fix
3. **Info / formatting** — fix opportunistically

### Step 6 — Present and wait

Present findings grouped by file and priority. Do **not** auto-fix unless the user explicitly asks.

```
src/foo/bar.ts
  Line 42  ERROR  @typescript-eslint/no-explicit-any
  Offending: function foo(x: any)
  Fix: function foo(x: FooInput)  // type lives in src/types/foo.types.ts:18

  Line 67  WARNING  no-console
  Offending: console.log('debug')
  Fix: remove or replace with logger.debug('debug')
```

Confirm before applying any fix. See `TOOL_RULES.md` — modification policy applies.

---

## Common patterns (language-agnostic)

- **`any` type** → find the correct type in the project's canonical sources (see `CANONICAL_SOURCES.md`)
- **`console.log`** → replace with the project's logger; check how other files log
- **Unused import** → remove it; check if it's used elsewhere before deleting
- **Missing `await`** → add it; check if the caller also needs to be async
- **Hardcoded magic value** → move to a named constant; grep for other usages of the same value
