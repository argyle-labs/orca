# Standard Delegation Patterns

Reference document for all agents. Agents reference this file instead of maintaining their own routing tables.

## Project knowledge bases

### BOD frontend (`~/code/rebuy_bod/bod`)
- **@bod-kb** — Patterns, conventions, codebase structure, component locations, auth flow, design system state
- **@bod-lint** — ESLint + TypeScript validation for BOD frontend changes
- **@bod-typecheck** — TypeScript strict mode for BOD frontend
- **@bod-cleanup** — Dead code, DS migration gaps, React pattern violations
- **@bod-optimize** — Re-renders, over-fetching, bundle size

### BOD API (`~/code/rebuy_bod/bod-api`)
- **@bod-api-kb** — Fastify/Kysely/Zod architecture, route patterns, Hivemind jobs, ice-age migrations, auth
- **@bod-api-lint** — ESLint + Prettier for BOD API changes
- **@bod-api-typecheck** — TypeScript for BOD API
- **@bod-api-migrate** — Database migration specialist (zero-downtime, FK changes, backfills)
- **@bod-api-test** — Jest integration tests against the running dev DB
- **@bod-api-docs** — Authoritative PostgreSQL 16 / Kysely / Fastify / Zod docs (fetched live)
- **@bod-api-review** — Full PR review with migration safety, data integrity, test coverage

### Shopify Connector (`~/code/rebuy_bod/bod-shopify-connector`)
- **@connector-kb** — Shopify OAuth, Hivemind jobs, Kysely patterns, iframe/bridge model
- **@connector-lint** — ESLint for Connector changes
- **@connector-typecheck** — TypeScript for Connector
- **@connector-migrate** — ice-age migration authoring and review
- **@connector-review** — Full PR review including auth edge cases and connector-bridge compatibility

### Rebuy platform (`~/code/rebuy`)
- **@rebuy-kb** — Top-level router: identifies the target repo and loads the right context skill
- **/rebuy-engine-context** (skill) — rebuyengine.com (PHP 5.x / CI2 / Webpack / K8s) context
- **/rebuy-db-context** (skill) — rebuy-db (MySQL / dbmate / sqlfluff) migration rules
- **/rebuy-cli-context** (skill) — rebuy-cli (Node.js / TypeScript / Commander.js) context
- **/rebuy-admin-nextjs-context** (skill) — admin-nextjs (Next.js / React / TailwindCSS) context
- **/rebuy-admin-api-context** (skill) — admin-api (PHP / CI4 / GraphQL) + RAI module context
- **/rebuy-onsite-context** (skill) — onsite-js (SDK / Webpack / React+Vue) context
- **/rebuy-installer-context** (skill) — installer (YAML / 1Password / rebuy-cli) env flow
- **@rebuy-deploy** — Bitbucket Pipelines deployment, K8s, environment tagging
- **@rebuy-migrate** — Full DB migration workflow (create → test → lint → commit → tag)

## Specialist agents

| Task | Agent |
|------|-------|
| Debug a bug, trace root cause | @fox |
| Read and explain code | @owl |
| Write or implement code | @crow |
| Simplify / reduce duplication | @spider |
| Code standards (any language) | @ferret |
| Critical review, gap-finding, system audit | @bear |
| Security audit | @viper |
| Test coverage audit | @shrew |
| Accessibility audit (WCAG 2.1 AA) | @swift |
| Cross-domain contract validation | @otter |
| External tech docs (TS, React, Postgres, etc.) | @elephant |
| Privacy / PII sweep | @hound |
| Coverage audit (missing agents/hooks) | @kestrel |
| PR comment formatting (Bitbucket/GitHub API) | @heron |
| Adversarial plan review | @mongoose |
| Homelab operations | @badger |
| DevOps / CI/CD / infra | @falcon |
| Note-taking / memory vault | @raven |
| Session logging / search across logs | @pinky |
| File reads, writes, finds, documentation | @pinky (delegates to owl/crow/raven/bloodhound/ibis) |
| Filesystem index + path resolution | @bloodhound |
| Documentation consistency | @ibis |
| Agent file maintenance | @wren |
| Placement auditing (wrong location) | @jackdaw |
| Scope graduation (project → global) | @magpie |
| Planning (minimal agent chain, token estimate) | @lynx |
| Escalation judgment (local vs Claude) | @osprey |
| Container inspection (running dev containers) | @hawk |
| Machine process / port inspection | @mole |
| BOD dev environment (carl CLI) | @boar |

## When to consult a KB agent

Before writing, refactoring, or reviewing code in any project — consult the KB agent first if you do not already have codebase context loaded. Grepping for patterns is not a substitute for understanding the architecture.

**Never guess at conventions. Read first.**
