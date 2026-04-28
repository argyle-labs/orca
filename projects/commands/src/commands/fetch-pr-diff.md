# Fetch Bitbucket PR Diff

Fetch a pull request diff from Bitbucket. The user must provide one of:

- A full Bitbucket URL: `https://bitbucket.org/rebuyengine/onsite-js/pull-requests/889`
- A repository and PR number: `onsite-js/889` or `onsite-js#889`

The repository must always be specified. Workspace defaults to "rebuyengine".

Execute the script with the user's input:

```bash
~/.claude/scripts/fetch-pr-diff.sh $ARGUMENTS
```

The script will:

1. Extract Bitbucket credentials from ~/.rebuy/config.yaml
2. Parse the input to determine workspace, repository, and PR number
3. Fetch PR metadata (title, author, description, branches)
4. Fetch the diff using the Bitbucket API
5. Save to ~/.rebuy/diffs/ with a timestamp
6. Output structured context for review

After fetching, report the saved diff file location and basic PR info (title, author, file count).
