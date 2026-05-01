# Canonical Sources

Where to find authoritative type, schema, and documentation sources per project. Reference this file instead of repeating source locations in each agent.

## BOD frontend (`~/code/rebuy_bod/bod`)

| What you need | Where it lives |
|---------------|----------------|
| Generated API client types | `~/app/lib/gen/types/` |
| API client (auto-generated, read-only) | `~/app/lib/gen/apiClient.generated.ts` |
| DS component props | `node_modules/@rebuy/components/` |
| Zod schemas | `src/<domain>/*.schema.ts` |
| Tailwind color tokens | CSS custom properties — never raw `primary-500` etc. |
| Auth patterns | Check `@bod-kb` — auth lives in `~/app/(auth)/` |

## BOD API (`~/code/rebuy_bod/bod-api`)

| What you need | Where it lives |
|---------------|----------------|
| Generated DB types | `src/types/database-generated.d.ts` — never hand-edit |
| Zod schemas / inferred types | `src/<domain>/*.schema.ts` |
| Kysely query patterns | Check `@bod-api-kb` or `@bod-api-docs` for live docs |
| Fastify route patterns | Check `@bod-api-kb` or `@bod-api-docs` |
| Hivemind job patterns | `src/backgroundJobs/` — check existing jobs |
| ice-age migration patterns | `src/db/migrations/` — check recent migrations |
| External docs (Postgres 16, Kysely, Fastify, Zod) | `@bod-api-docs` fetches live |

## Shopify Connector (`~/code/rebuy_bod/bod-shopify-connector`)

| What you need | Where it lives |
|---------------|----------------|
| Generated DB types | `src/types/database-generated.d.ts` |
| Shopify OAuth flow | `@connector-kb` — lives in `src/app/api/auth/` |
| Connector bridge model | `@connector-kb` — iframe/postMessage patterns |
| Hivemind job patterns | `src/jobs/` |
| ice-age migration patterns | `src/db/migrations/` |

## Rebuy platform (`~/code/rebuy`)

| What you need | Where it lives |
|---------------|----------------|
| rebuyengine.com architecture | `/rebuy-engine-context` skill + `rebuyengine.com/CLAUDE.md` |
| Database migration rules | `/rebuy-db-context` skill + `rebuy-db/CLAUDE.md` |
| CLI commands and patterns | `/rebuy-cli-context` skill + `rebuy-cli/CLAUDE.md` |
| admin-nextjs patterns | `/rebuy-admin-nextjs-context` skill + `admin-nextjs/CLAUDE.md` |
| admin-api / RAI module | `/rebuy-admin-api-context` skill + `admin-api/docs/rai/CLAUDE.md` |
| onsite-js SDK structure | `/rebuy-onsite-context` skill + `onsite-js/package.json` |
| Env setup / installer flow | `/rebuy-installer-context` skill + `installer/README.md` |

## External documentation

| Technology | How to get it |
|------------|---------------|
| PostgreSQL 16 | `@bod-api-docs` or `@elephant` |
| Kysely | `@bod-api-docs` or `@elephant` |
| Fastify | `@bod-api-docs` or `@elephant` |
| Zod | `@bod-api-docs` or `@elephant` |
| TypeScript | `@elephant` |
| React / Next.js | `@elephant` |
| Shopify API | `@connector-kb` or `@elephant` |

## Hard rules

- **Never hand-edit generated files** (`*.generated.ts`, `database-generated.d.ts`). They are regenerated and your changes will be lost. Fix the generator or migration instead.
- **Never guess at types.** Look them up in the canonical sources above. If a type does not exist, that is the real finding — not a type cast.
