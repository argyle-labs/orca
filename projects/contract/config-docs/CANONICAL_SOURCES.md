# Canonical Sources

Where to find authoritative type, schema, and documentation sources per project. Reference this file instead of repeating source locations in each agent.

## Example frontend (`~/code/example-org/example-app`)

| What you need | Where it lives |
|---------------|----------------|
| Generated API client types | `~/app/lib/gen/types/` |
| API client (auto-generated, read-only) | `~/app/lib/gen/apiClient.generated.ts` |
| Design-system component props | `node_modules/@example-org/components/` |
| Schema definitions | `src/<domain>/*.schema.ts` |
| Color tokens | CSS custom properties — never raw `primary-500` etc. |
| Auth patterns | Check `@app-kb` — auth lives in `~/app/(auth)/` |

## Example API (`~/code/example-org/example-app-api`)

| What you need | Where it lives |
|---------------|----------------|
| Generated DB types | `src/types/database-generated.d.ts` — never hand-edit |
| Schema definitions / inferred types | `src/<domain>/*.schema.ts` |
| Query-builder patterns | Check `@app-api-kb` or `@app-api-docs` for live docs |
| Route patterns | Check `@app-api-kb` or `@app-api-docs` |
| Background-job patterns | `src/backgroundJobs/` — check existing jobs |
| Migration patterns | `src/db/migrations/` — check recent migrations |
| External docs (Postgres, query builder, web framework, schema lib) | `@app-api-docs` fetches live |

## Example service (`~/code/example-org/example-service`)

| What you need | Where it lives |
|---------------|----------------|
| Generated DB types | `src/types/database-generated.d.ts` |
| OAuth flow | `@service-kb` — lives in `src/app/api/auth/` |
| Bridge model | `@service-kb` — iframe/postMessage patterns |
| Background-job patterns | `src/jobs/` |
| Migration patterns | `src/db/migrations/` |

## Example platform (`~/code/example-platform`)

| What you need | Where it lives |
|---------------|----------------|
| Web app architecture | `/platform-engine-context` skill + `engine/CLAUDE.md` |
| Database migration rules | `/platform-db-context` skill + `platform-db/CLAUDE.md` |
| CLI commands and patterns | `/platform-cli-context` skill + `platform-cli/CLAUDE.md` |
| Admin frontend patterns | `/platform-admin-context` skill + `admin/CLAUDE.md` |
| Admin API patterns | `/platform-admin-api-context` skill + `admin-api/CLAUDE.md` |
| SDK structure | `/platform-sdk-context` skill + `sdk/package.json` |
| Env setup / installer flow | `/platform-installer-context` skill + `installer/README.md` |

## External documentation

| Technology | How to get it |
|------------|---------------|
| PostgreSQL | `@app-api-docs` or `@elephant` |
| Query builder | `@app-api-docs` or `@elephant` |
| Web framework | `@app-api-docs` or `@elephant` |
| Schema library | `@app-api-docs` or `@elephant` |
| TypeScript | `@elephant` |
| React / Next.js | `@elephant` |
| Service API | `@service-kb` or `@elephant` |

## Hard rules

- **Never hand-edit generated files** (`*.generated.ts`, `database-generated.d.ts`). They are regenerated and your changes will be lost. Fix the generator or migration instead.
- **Never guess at types.** Look them up in the canonical sources above. If a type does not exist, that is the real finding — not a type cast.
