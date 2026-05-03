# Brain secrets template — loaded via 1Password CLI at runtime.
# Run with: op run --env-file .env.brain.tpl -- <command>
#
# All secrets live in 1Password: personal account → automations vault → "brain" item.
# To fill in placeholders: op item edit brain --vault automations --account scottdkey@gmail.com
#
# Fields to populate:
#   github_token              — classic PAT with repo scope on scottdkey/brain
#   github_oauth_client_id    — GitHub OAuth App client ID (for `brain login github`)
#   atlassian_oauth_client_id — Atlassian OAuth App client ID (for `brain login atlassian`)
#   atlassian_oauth_client_secret — Atlassian OAuth App client secret

ANTHROPIC_API_KEY=op://automations/brain/anthropic_api_key
GITHUB_TOKEN=op://automations/brain/github_token
GITHUB_OAUTH_CLIENT_ID=op://automations/brain/github_oauth_client_id
ATLASSIAN_OAUTH_CLIENT_ID=op://automations/brain/atlassian_oauth_client_id
ATLASSIAN_OAUTH_CLIENT_SECRET=op://automations/brain/atlassian_oauth_client_secret
