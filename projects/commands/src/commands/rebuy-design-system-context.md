# rebuy-design-system-context

Load context for the **rebuy-design-system** monorepo — the canonical Rebuy component library and design tokens, consumed by `admin-nextjs` and other frontends as `@rebuy/components` and `@rebuy/design-tokens`.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/rebuy-design-system/CLAUDE.md
Read: ~/code/rebuy/rebuy-design-system/packages/components/README.md (first 60 lines)
Read: ~/code/rebuy/rebuy-design-system/packages/components/ds-reference.md (first 100 lines)
```

## Step 2 — Orient in the monorepo

```bash
ls ~/code/rebuy/rebuy-design-system/                       # apps, packages, scripts, docs
ls ~/code/rebuy/rebuy-design-system/packages/              # components, tokens
ls ~/code/rebuy/rebuy-design-system/packages/components/src/   # component source
ls ~/code/rebuy/rebuy-design-system/packages/tokens/src/   # token definitions
cat ~/code/rebuy/rebuy-design-system/package.json          # workspace scripts
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- **Monorepo:** pnpm workspaces + Turbo
- **Versioning:** changesets (`pnpm changeset`); `@rebuy/components` and `@rebuy/design-tokens` are linked — they version together
- **Components:** React 18+, TypeScript, CVA (class-variance-authority), Radix primitives, shadcn/ui patterns
- **Tokens:** generated from `packages/tokens/src/data/source-data/token-definitions.json` (single source of truth)
- **Build:** Vite (components), `ts-morph` AST + Playwright DOM blueprint generators
- **Storybook:** `@rebuy/gallery` app for visual review
- **Governance:** `@rebuy/governance` app for design-system QA

### Workspace layout
- `packages/components/` → `@rebuy/components` (publishable, restricted npm access)
- `packages/tokens/` → `@rebuy/design-tokens`
- `apps/gallery/` → `@rebuy/gallery` (Storybook, not published)
- `apps/governance/` → `@rebuy/governance` (internal QA app, not published)
- `figma-plugin/` → Figma plugin source (Genesis blueprint pipeline)

### Critical rules (from CLAUDE.md)
- **NEVER edit generated files:** `packages/tokens/src/tokens/colors.ts`, `packages/tokens/dist/css/variables.css`, `packages/tokens/dist/json/tokens-flat.json`. Edit `token-definitions.json` and regenerate via `pnpm --filter @rebuy/tokens build:colors` then `pnpm --filter @rebuy/tokens build`.
- **Every PR touching `packages/` requires a changeset.** CI blocks merges without one. Run `pnpm changeset` interactively.
- **Code-first design system.** Git owns structure, tokens, components. Figma reads from Git via the Genesis blueprint and proposes changes through a plugin-to-PR pipeline.

### Component lookup
`packages/components/ds-reference.md` is auto-generated and acts as a "need → component" index. Regenerate with `pnpm generate:ds-reference`.

### Consumer relationship to admin-nextjs
- Published as `@rebuy/components` (currently `0.2.3` in admin-nextjs).
- Imported directly: `import { Button, Input, Checkbox } from '@rebuy/components'`.
- Adoption is **incremental** in admin-nextjs — most of the app still uses raw Tailwind with custom scales. New components should prefer DS primitives when available.
- Tailwind config in admin-nextjs (`apps/nextjs/tailwind.config.ts`) defines its own custom color scales that overlap with — but are not yet driven by — `@rebuy/design-tokens`. Token unification is an open migration.

### Common scripts
```bash
pnpm dev                                       # turbo dev across packages
pnpm build                                     # full build (runs contrast audit first)
pnpm storybook                                 # local Storybook (gallery)
pnpm governance                                # governance QA app
pnpm changeset                                 # create a changeset for a PR
pnpm generate:blueprint                        # AST + DOM blueprints for Figma seeding
pnpm generate:ds-reference                     # regenerate ds-reference.md
pnpm --filter @rebuy/tokens build:colors       # regenerate colors.ts from JSON
pnpm --filter @rebuy/tokens build              # regenerate all token outputs
pnpm --filter @rebuy/components build          # build component package
```

### Publishing
Manual Bitbucket pipeline trigger (`publish-npm`). The pipeline runs `scripts/release-with-changesets.sh`: version → commit → publish → push tags. Figma token submissions auto-generate patch changesets via the submit worker.

## Step 4 — Hand off

Surface the summary and stop. Wait for the user's next instruction before reading additional files.
