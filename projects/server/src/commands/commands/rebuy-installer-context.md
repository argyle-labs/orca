# rebuy-installer-context

Load context for the **installer** repository — central configuration for local dev environments.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/installer/README.md
```

## Step 2 — Read key config files

```bash
cat ~/code/rebuy/installer/config.yaml          # vault mappings and shared secrets
cat ~/code/rebuy/installer/.env.common          # shared env template
ls ~/code/rebuy/installer/templates/            # per-project .env templates
ls ~/code/rebuy/installer/contracts/            # YAML validation schemas
```

## Step 3 — Surface structured context

Return a summary with these sections:

### What it is
- Central configuration repository for local dev environments
- Works with `rebuy-cli` to generate `.env` files with 1Password integration

### How it works
1. `config.yaml` maps vault paths to shared secret references
2. `.env.common` is the base template using `${SHARED:...}` syntax
3. `templates/` has per-project overrides for each repo
4. `contracts/` validates the generated env shape with YAML schemas
5. `rebuy-cli` reads this config and pulls secrets from 1Password

### Per-project templates
List what's in `templates/`: which projects have env templates (admin-api, apiv2, admin-nextjs, onsite-js, rebuyengine.com, etc.)

### Secret reference syntax
- `${SHARED:path.to.secret}` — pulls from shared vault
- `${LOCAL:VAR_NAME}` — uses local machine value

### Contracts (validation schemas)
- Each project has a YAML schema defining required env vars
- rebuy-cli validates generated `.env` against the contract before writing

### Common operations
- Generate env for a project: `rebuy env setup <project>` (via rebuy-cli)
- Add a new secret: edit `config.yaml` and the relevant template
- Validate: rebuy-cli runs contract validation automatically

---

**This skill is invoked by `@rebuy-kb` and `/rebuy-env`. The installer is the single source of truth for dev environment configuration — any env question about any rebuy project starts here.**
