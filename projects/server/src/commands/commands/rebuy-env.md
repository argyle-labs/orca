# rebuy-env

Set up or inspect a local development environment for a rebuy project.

Use the **rebuy-cli MCP server** for all rebuy operations — do not run `rebuy` bash commands directly.

---

## Step 1 — Identify the target project

Ask (or read from context):
- Which project? (rebuyengine.com, admin-api, admin-nextjs, apiv2, onsite-js, rebuy-cli)
- Is this a fresh setup or updating an existing env?

## Step 2 — Check prerequisites via MCP

Call `rebuy_doctor` to verify the environment is healthy and identify any missing dependencies.

Call `rebuy_auth_status` to confirm 1Password CLI auth is active.

Call `rebuy_auth_op_status` if auth_status shows issues.

## Step 3 — Check current env state

Call `rebuy_env_current` to see which env profile is active.

Call `rebuy_env_status` to see whether services are running.

## Step 4 — Generate or update the environment

Call `rebuy_env_generate` to create or refresh the `.env` file for the project.

Call `rebuy_env_validate` to verify all required variables are populated.

## Step 5 — Start the environment

Call `rebuy_db_up` to ensure the database container is running.

Call `rebuy_env_start` to start the project's services.

Call `rebuy_env_status` again to confirm everything came up.

## Step 6 — Report

Summarize: what was set up, any warnings from validate or doctor, next step.

---

**For DNS setup:** use `rebuy_env_dns_dev` or `rebuy_dns_dev`. For env switching: `rebuy_env_switch`.**
