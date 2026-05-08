# hackernews — orca example plugin (TypeScript)

Polls the public [Hacker News API](https://github.com/HackerNews/API) for
the current top story list every minute, fetches the top 10 items, and
publishes each as a `Story` TypedValue into the `news:hn` context.

No auth required.

## Build

```sh
cd projects/examples/hackernews
npm install
npm run build
```

## Run standalone

```sh
orca pki issue hackernews
ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
ORCA_PKI_DIR=$HOME/.orca/pki \
ORCA_PLUGIN_ID=hackernews \
node dist/index.js
```

Published TypedValue:

```json
{
  "type": "hackernews.Story",
  "schema_version": "0.1.0",
  "sensitivity": "general",
  "payload": {
    "id": 99999999,
    "title": "Show HN: orca plugin federation",
    "by": "scottkey",
    "url": "https://example.com/article",
    "score": 142,
    "descendants": 38,
    "time": 1715212345
  }
}
```
