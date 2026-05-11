# rebuy-pr

Create a Bitbucket PR for the current rebuy project branch.

Use the **rebuy-cli MCP server** — do not run `rebuy pr` bash commands directly.

---

## Step 1 — Gather context via MCP

Call `rebuy_pr_context` to get the current branch, commit summary, and any project-specific PR metadata the CLI knows about.

## Step 2 — Safety check

If the branch touches `db/migrations/`: invoke `/rebuy-db-context` to confirm migration safety rules before proceeding.

If the branch touches rebuyengine.com invariants: invoke `/rebuy-engine-context`.

## Step 3 — Create the PR

Call `rebuy_pr_create` with a clear title and description.

Description checklist:
- **What changed** — one sentence
- **Why** — ticket or motivation
- **How to test** — specific steps a reviewer can follow
- **Migration notes** — if any DB migrations are included
- **Screenshots** — if UI changed

## Step 4 — Report

Return the PR URL. Note any reviewers or labels the user should assign manually.

---

**Confirm scope with the user before creating. PRs are visible to others.**
