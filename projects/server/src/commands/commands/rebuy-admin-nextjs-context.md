# rebuy-admin-nextjs-context

Load context for the **admin-nextjs** codebase — the Next.js admin dashboard.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/admin-nextjs/CLAUDE.md
Read: ~/code/rebuy/admin-nextjs/README.md (first 60 lines)
```

## Step 2 — Orient in the codebase

```bash
ls ~/code/rebuy/admin-nextjs/apps/nextjs/app/     # main app source (Next.js App Router)
ls ~/code/rebuy/admin-nextjs/apps/nextjs/          # package.json, config
cat ~/code/rebuy/admin-nextjs/apps/nextjs/package.json  # deps and scripts
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- Next.js (App Router), React, TypeScript, TailwindCSS, SWR
- Package root: `./apps/nextjs`

### Architecture (from CLAUDE.md)
- Domain-driven structure: smart-flows, smartcart, experiments, etc.
- **Absolute imports only** — no relative paths between feature areas
- **No barrel files** — direct imports only

### Key conventions (quote from CLAUDE.md)
- File naming: kebab-case
- Component naming: PascalCase
- Absolute imports via configured aliases
- Zod schemas for validation
- Custom Tailwind colors (check config before using any color)

### Domain areas
List top-level domain directories from `app/` scan

### Dev workflow
- Start: check README for dev command
- Lint: check package.json scripts
- Build: check package.json scripts

---

**This skill is invoked by `@rebuy-kb`. The admin-nextjs dashboard is incrementally replacing older UI — when in doubt about a pattern, check the CLAUDE.md for the authoritative convention.**
