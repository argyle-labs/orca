# Fetch Bitbucket PR Diff

Fetch a pull request diff from Bitbucket. The user must provide one of:

- A full Bitbucket URL: `https://bitbucket.org/my-workspace/example-web/pull-requests/889`
- A repository and PR number: `example-web/889` or `example-web#889`

The repository must always be specified. Workspace defaults to your configured default workspace.

Execute the script with the user's input:

```bash
~/.claude/scripts/fetch-pr-diff.sh $ARGUMENTS
```

The script will:

1. Extract Bitbucket credentials from the configured credentials file
2. Parse the input to determine workspace, repository, and PR number
3. Fetch PR metadata (title, author, description, branches)
4. Fetch the diff using the Bitbucket API
5. Save to the local diffs directory with a timestamp
6. Output structured context for review

After fetching, report the saved diff file location and basic PR info (title, author, file count).
