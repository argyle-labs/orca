# github-releases — orca example plugin (Kotlin)

Polls the [GitHub Releases API](https://docs.github.com/en/rest/releases/releases)
for one or more repos every 5 minutes and publishes new releases as `Release`
TypedValues into `dev:releases`. The first poll for each repo is treated as
the baseline — only releases that show up in subsequent polls are emitted.

## Build

```sh
cd projects/examples/github-releases
gradle appJar
```

The fat-jar lands at `build/libs/orca-example-github-releases-*-all.jar`.

## Run standalone

```sh
orca pki issue github-releases
ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
ORCA_PKI_DIR=$HOME/.orca/pki \
ORCA_PLUGIN_ID=github-releases \
GITHUB_REPOS="rust-lang/rust,denoland/deno" \
GITHUB_TOKEN=ghp_...   # optional but raises rate limit 60→5000/hr
java -jar build/libs/orca-example-github-releases-*-all.jar
```

Published TypedValue:

```json
{
  "type": "github-releases.Release",
  "schema_version": "0.1.0",
  "sensitivity": "general",
  "payload": {
    "repo": "rust-lang/rust",
    "id": 12345678,
    "tag_name": "1.95.0",
    "name": "Rust 1.95.0",
    "html_url": "https://github.com/rust-lang/rust/releases/tag/1.95.0",
    "draft": false,
    "prerelease": false,
    "published_at": "2026-04-15T10:00:00Z"
  }
}
```
