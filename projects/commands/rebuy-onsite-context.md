# rebuy-onsite-context

Load context for the **onsite-js** codebase — the client-side JavaScript SDK.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```bash
cat ~/code/rebuy/onsite-js/package.json
```

No top-level README — check `docs/` if documentation is needed.

## Step 2 — Orient in the codebase

```bash
ls ~/code/rebuy/onsite-js/src/             # main SDK source
ls ~/code/rebuy/onsite-js/src/ | head -20  # top-level structure
ls ~/code/rebuy/onsite-js/                 # config files, webpack config
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- Node.js, TypeScript, Webpack, Sass, Jest
- React and Vue components (embedded SDK)
- ESLint + Stylelint

### What it is
- Data-driven personalization and merchandising SDK
- Ships as a publishable JS bundle to merchant storefronts
- Includes: Smart Flows, Smart Cart, embedded components

### Architecture
- Entry: `src/` → Webpack builds to publishable SDK
- Output: built artifacts (check webpack config for output dir)
- Components: React and Vue embedded widgets

### Dev commands (from package.json)
- `npm run watch` — watch mode
- `npm run build` — production build
- `npm test` — Jest test suite
- `npm run lint` — ESLint + Stylelint
- `npm run format` — auto-fix lint issues

### Docs generation
- JSDoc + markdown → check package.json for docs script

### Key constraints
- This is a **client-side SDK** — bundle size matters
- Changes affect merchant storefronts directly — no staging environment buffer
- Must support multiple framework integrations (React, Vue, plain JS)

---

**This skill is invoked by `@rebuy-kb`. The onsite-js SDK ships directly to merchant sites — treat all changes with production-first caution.**
