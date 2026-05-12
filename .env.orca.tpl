# Orca secrets template — loaded via 1Password CLI at runtime.
# Run with: op run --account "$OP_ACCOUNT_PERSONAL" --env-file .env.orca.tpl -- <command>
#
# All secrets live in 1Password.
# GITHUB_TOKEN      — op://Coding/github.com/token  (personal PAT with repo scope)
# ANTHROPIC_API_KEY — op://automations/brain/anthropic_api_key
# OAuth creds       — op://automations/brain/{github,atlassian}_oauth_*

GITHUB_TOKEN=op://Coding/github.com/token
ANTHROPIC_API_KEY=op://automations/brain/anthropic_api_key
GITHUB_OAUTH_CLIENT_ID=op://automations/brain/github_oauth_client_id
ATLASSIAN_OAUTH_CLIENT_ID=op://automations/brain/atlassian_oauth_client_id
ATLASSIAN_OAUTH_CLIENT_SECRET=op://automations/brain/atlassian_oauth_client_secret
