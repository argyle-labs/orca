# rebuy-admin-api-context

Load context for the **admin-api** or **apiv2** codebase — PHP/CI4 API + the R/AI module.

Both repos share the same CI4 stack and directory structure. When invoked for **apiv2**, substitute `apiv2` for `admin-api` in all paths below (e.g. `~/code/rebuy/apiv2/apps/ci4/app/Controllers/`). The R/AI docs only exist in `admin-api`.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/admin-api/README.md
Read: ~/code/rebuy/admin-api/docs/rai/CLAUDE.md
```

## Step 2 — Orient in the codebase

```bash
ls ~/code/rebuy/admin-api/apps/ci4/app/             # CI4 app structure
ls ~/code/rebuy/admin-api/apps/ci4/app/Controllers/
ls ~/code/rebuy/admin-api/docs/rai/                 # R/AI module docs
ls ~/code/rebuy/admin-api/docs/rai/features/        # feature docs
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- PHP, CodeIgniter 4, GraphQL
- Docker, Bitbucket Pipelines
- Google Cloud: Cloud SQL, Redis, Cloud Logging
- Shopify API integration

### Architecture
- CI4 MVC: `apps/ci4/app/Controllers/`, `apps/ci4/app/Models/`, `apps/ci4/app/Libraries/`
- GraphQL layer: `apps/ci4/app/Graphs/` (type definitions) + `apps/ci4/app/Mutations/`
- CI4 config: `apps/ci4/app/Config/`

### R/AI Module
- Purpose: LLM-powered features (Executive Summary, ranking engine)
- Key files:
  - `Config/ExecutiveSummary.php` — LLM prompts (system + instruction)
  - `Config/InsightFeatureRegistry.php` — feature→package→path mapping
  - `Services/ExecutiveSummaryService.php` — ranking engine orchestration
- Docs: `docs/rai/features/executive-summary/` (CHANGELOG, ROADMAP, DATA_FLOW, TEST_PAYLOADS)

### Dev workflow
- Docker-based local development (see README)
- Postman collection available for API testing

### Integration points
- Shopify API: webhook and API calls
- Cloud SQL: primary database
- Redis: caching layer
- Cloud Logging: structured logging

---

**This skill is invoked by `@rebuy-kb`. The R/AI module is a growing feature area — check `docs/rai/CLAUDE.md` for LLM-specific patterns before touching any AI-related code.**
