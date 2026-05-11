# rebuy-db-context

Load context for the **rebuy-db** codebase — MySQL schema management and migrations.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/rebuy-db/CLAUDE.md
Read: ~/code/rebuy/rebuy-db/README.md (first 60 lines)
```

## Step 2 — Orient in the codebase

```bash
ls ~/code/rebuy/rebuy-db/db/migrations/ | tail -10   # 10 most recent migrations
ls ~/code/rebuy/rebuy-db/                              # top-level structure
cat ~/code/rebuy/rebuy-db/Makefile                     # available commands
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- MySQL, dbmate, sqlfluff, Docker, Makefile

### Migration workflow
Quote the full workflow from CLAUDE.md:
1. Create: `docker run ... dbmate new <name>`
2. Edit SQL (migrate:up / migrate:down sections)
3. Test: `./test.sh` (isolated Docker MySQL on port 3307)
4. Lint: `docker run ... sqlfluff lint`
5. Commit and push
6. Deploy: tag with `rc-*` (staging) or `rebuy-db-*` (production)

### Naming convention
`YYYYMMDDHHMMSS_REB-XXXXX-descriptive-name.sql`

### Safety rules (ALWAYS surface these — non-negotiable)
Quote directly from CLAUDE.md:
- ❌ NEVER run migrations against production directly
- ⚠️ Only run against staging with explicit user approval
- ✅ Safe operations: creating migrations, linting, local testing with `test.sh`
- ✅ Production migrations ONLY via Bitbucket Pipelines with tagging

### Schema location
- Migrations: `db/migrations/` (~235+ files)
- Schema: `db/schema.sql` (auto-generated, read-only)

---

**This skill is invoked by `@rebuy-kb` and `@rebuy-migrate`. Always surface the safety rules — running migrations against production directly is a critical incident.**
