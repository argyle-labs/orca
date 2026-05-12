# Orca secrets template — loaded via 1Password CLI at runtime.
# Run with: op run --account "$OP_ACCOUNT_PERSONAL" --env-file .env.orca.tpl -- <command>
#
# All secrets consolidated under op://automations/orca/. Daily-driver GitHub
# auth goes through `gh auth login` (OAuth → keyring); scripts call
# `gh auth token` when they need an Authorization header — no static
# GITHUB_TOKEN here, so direnv can't override gh's keyring with an expired PAT.
#
# Remote install.sh runs receive the token over SSH from `gh auth token` on
# the orchestrating host (see scripts/install.sh + how thor was provisioned).

ANTHROPIC_API_KEY=op://automations/orca/anthropic_api_key
ATLASSIAN_TOKEN=op://automations/orca/atlassian_token
