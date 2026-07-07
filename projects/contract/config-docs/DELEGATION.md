# Standard Delegation Patterns

Reference document for all agents. Agents reference this file instead of maintaining their own routing tables.

## Project knowledge bases

### Example frontend (`~/code/example-org/example-app`)
- **@app-kb** — Patterns, conventions, codebase structure, component locations, auth flow, design system state
- **@app-lint** — ESLint + TypeScript validation for frontend changes
- **@app-typecheck** — TypeScript strict mode for the frontend
- **@app-cleanup** — Dead code, design-system migration gaps, React pattern violations
- **@app-optimize** — Re-renders, over-fetching, bundle size

### Example API (`~/code/example-org/example-app-api`)
- **@app-api-kb** — Web-framework/query-builder/schema architecture, route patterns, background jobs, migrations, auth
- **@app-api-lint** — ESLint + Prettier for API changes
- **@app-api-typecheck** — TypeScript for the API
- **@app-api-migrate** — Database migration specialist (zero-downtime, FK changes, backfills)
- **@app-api-test** — Integration tests against the running dev DB
- **@app-api-docs** — Authoritative database / query-builder / web-framework / schema docs (fetched live)
- **@app-api-review** — Full PR review with migration safety, data integrity, test coverage

### Example service (`~/code/example-org/example-service`)
- **@service-kb** — OAuth, background jobs, query patterns, iframe/bridge model
- **@service-lint** — ESLint for service changes
- **@service-typecheck** — TypeScript for the service
- **@service-migrate** — Migration authoring and review
- **@service-review** — Full PR review including auth edge cases and bridge compatibility

### Example platform (`~/code/example-platform`)
- **@platform-kb** — Top-level router: identifies the target repo and loads the right context skill
- **/platform-engine-context** (skill) — web app engine context
- **/platform-db-context** (skill) — database migration rules
- **/platform-cli-context** (skill) — CLI (Node.js / TypeScript) context
- **/platform-admin-context** (skill) — admin frontend (Next.js / React) context
- **/platform-admin-api-context** (skill) — admin API context
- **/platform-sdk-context** (skill) — SDK context
- **/platform-installer-context** (skill) — installer env flow
- **@platform-deploy** — CI/CD deployment, container orchestration, environment tagging
- **@platform-migrate** — Full DB migration workflow (create → test → lint → commit → tag)

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
| Session logging / search across logs | @otter |
| File reads, writes, finds, documentation | @otter (delegates to owl/crow/raven/bloodhound/ibis) |
| Filesystem index + path resolution | @bloodhound |
| Documentation consistency | @ibis |
| Agent file maintenance | @wren |
| Placement auditing (wrong location) | @jackdaw |
| Scope graduation (project → global) | @magpie |
| Planning (minimal agent chain, token estimate) | @lynx |
| Escalation judgment (local vs Claude) | @osprey |
| Container inspection (running dev containers) | @hawk |
| Machine process / port inspection | @mole |
| Dev environment setup | @boar |

## When to consult a KB agent

Before writing, refactoring, or reviewing code in any project — consult the KB agent first if you do not already have codebase context loaded. Grepping for patterns is not a substitute for understanding the architecture.

**Never guess at conventions. Read first.**
