# rebuy-migrate

Create and deploy a database migration for the rebuy-db project.

Use the **rebuy-cli MCP server** for rebuy operations. The dbmate steps (create, test, lint) require Docker and run as bash commands — those stay as-is.

---

## Step 1 — Load migration context

Invoke `/rebuy-db-context` to confirm current safety rules and workflow.

## Step 2 — Check DB state via MCP

Call `rebuy_db_status` to confirm the local database container is running.

Call `rebuy_db_health` to verify the database is healthy before touching migrations.

If the DB is down: call `rebuy_db_up` to start it, then re-check health.

## Step 3 — Create the migration file (Docker/dbmate)

```bash
cd ~/code/rebuy/rebuy-db && docker run --rm -v "$(pwd)/db:/db" ghcr.io/amacneil/dbmate new REB-XXXXX-descriptive-name
```

Naming: `YYYYMMDDHHMMSS_REB-XXXXX-descriptive-name.sql`

## Step 4 — Write the migration

Edit `db/migrations/<filename>.sql`:

```sql
-- migrate:up
-- Your migration SQL here

-- migrate:down
-- Rollback SQL (or comment explaining why it's destructive)
```

Safety checklist:
- [ ] NOT NULL columns have a default or are nullable during transition
- [ ] No bulk data UPDATE inside the migration
- [ ] Multiple ALTERs on the same table combined into one statement
- [ ] No explicit database name references (`rebuy.table` → just `table`)

## Step 5 — Test locally (Docker/dbmate)

```bash
cd ~/code/rebuy/rebuy-db && ./test.sh
```

Runs against an isolated Docker MySQL on port 3307. Never test against staging or production directly.

## Step 6 — Lint (Docker/SQLFluff)

```bash
cd ~/code/rebuy/rebuy-db && docker run --rm -v "$(pwd):/sql" sqlfluff/sqlfluff:3.2.5 lint /sql/db/migrations/<filename>.sql --dialect mysql
```

## Step 7 — Apply to local DB via MCP (optional, for development)

Call `rebuy_db_migrate` to apply pending migrations to the local dev database.

## Step 8 — Commit and deploy

Stage only the migration file, then tell the user to commit and push.

**Staging:** Tag with `rc-*` → triggers staging pipeline
**Production:** Tag with `rebuy-db-*` → triggers production pipeline

**Never run migrations manually against production or staging without explicit user approval.**
