# rebuy-engine-context

Load context for the **rebuyengine.com** codebase — the core Rebuy platform.

When this skill is invoked, read the following and surface a structured context summary:

## Step 1 — Read primary docs

```
Read: ~/code/rebuy/rebuyengine.com/CLAUDE.md
Read: ~/code/rebuy/rebuyengine.com/README.md (first 80 lines)
```

## Step 2 — Orient in the codebase

Run these to understand current structure:

```bash
ls ~/code/rebuy/rebuyengine.com/application/
ls ~/code/rebuy/rebuyengine.com/application/controllers/
ls ~/code/rebuy/rebuyengine.com/application/models/ | head -30
ls ~/code/rebuy/rebuyengine.com/webpack/src/
```

## Step 3 — Surface structured context

Return a summary with these sections:

### Stack
- PHP version, CodeIgniter 2, Webpack version
- Docker setup, Kubernetes, GCP

### Architecture
- MVC structure: controllers → models → views
- Key directories and what they own
- Frontend: Webpack entry, output location

### Critical invariants (ALWAYS surface these — safety-critical)
From CLAUDE.md — do not paraphrase, quote directly:
1. `is_in_stock` behavior (key absent vs true vs false/null)
2. Cache exclusion filter order (Cache_ProductCollection_CustomApiEndPointCache)

### Dev commands
- Start Docker, run tests, lint

### Where to look
- For a business rule: `application/controllers/`
- For data access: `application/models/`
- For custom services: `application/libraries/`
- For frontend: `webpack/src/`
- For K8s config: `k8s/`

---

**This skill is invoked by `@rebuy-kb` and any agent that needs rebuyengine.com context. Always surface the critical invariants — they are safety rules that cause real bugs when violated.**
