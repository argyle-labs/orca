/**
 * hackernews — orca plugin example.
 *
 * Polls the public Hacker News Firebase API for the current top story list,
 * fetches the top N stories, and publishes each as a `Story` TypedValue
 * into the `news:hn` context.
 *
 * Standalone dev:
 *
 *   ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
 *   ORCA_PKI_DIR=$HOME/.orca/pki \
 *   ORCA_PLUGIN_ID=hackernews \
 *   node dist/index.js
 *
 * API reference: https://github.com/HackerNews/API
 */
import { pki, Transport } from '@orca/sdk';
const TYPE_NAME = 'Story';
const SCHEMA_VERSION = '0.1.0';
const CONTEXT_ID = 'news:hn';
const POLL_INTERVAL_MS = 60_000;
const TOP_N = 10;
const SCHEMA = {
    type: 'object',
    properties: {
        id: { type: 'integer' },
        title: { type: 'string' },
        by: { type: 'string' },
        url: { type: 'string' },
        score: { type: 'integer' },
        descendants: { type: 'integer' },
        time: { type: 'integer' },
    },
    required: ['id', 'title', 'time'],
};
function envRequired(name) {
    const v = process.env[name];
    if (!v)
        throw new Error(`required env var ${name} not set`);
    return v;
}
async function fetchTopStoryIDs() {
    const r = await fetch('https://hacker-news.firebaseio.com/v0/topstories.json');
    if (!r.ok)
        throw new Error(`HN topstories ${r.status}`);
    return (await r.json());
}
async function fetchStory(id) {
    const r = await fetch(`https://hacker-news.firebaseio.com/v0/item/${id}.json`);
    if (!r.ok)
        throw new Error(`HN item ${id} ${r.status}`);
    return (await r.json());
}
async function main() {
    const addr = envRequired('ORCA_PLUGIN_ADDR');
    const pkiDir = envRequired('ORCA_PKI_DIR');
    const pluginID = envRequired('ORCA_PLUGIN_ID');
    const bundle = await pki.loadPlugin(pkiDir, pluginID);
    const transport = await Transport.connect({ addr, bundle });
    await transport.hello(pluginID, 'headless');
    await transport.declareTypes([
        { type_name: TYPE_NAME, schema_version: SCHEMA_VERSION, schema: SCHEMA, sensitivity: 'general' },
    ]);
    const typeID = `${pluginID}.${TYPE_NAME}`;
    const tick = async () => {
        let ids;
        try {
            ids = (await fetchTopStoryIDs()).slice(0, TOP_N);
        }
        catch (err) {
            process.stderr.write(`hn topstories: ${err.message}\n`);
            return;
        }
        for (const id of ids) {
            let story;
            try {
                story = await fetchStory(id);
            }
            catch (err) {
                process.stderr.write(`hn item ${id}: ${err.message}\n`);
                continue;
            }
            if (!story || !story.id)
                continue;
            const value = {
                type: typeID,
                schema_version: SCHEMA_VERSION,
                sensitivity: 'general',
                payload: {
                    id: story.id,
                    title: story.title,
                    by: story.by,
                    url: story.url ?? `https://news.ycombinator.com/item?id=${story.id}`,
                    score: story.score,
                    descendants: story.descendants ?? 0,
                    time: story.time,
                },
            };
            await transport.publishContext(CONTEXT_ID, value);
        }
    };
    await tick();
    const interval = setInterval(() => {
        void tick();
    }, POLL_INTERVAL_MS);
    const shutdown = () => {
        clearInterval(interval);
        transport.close();
        process.exit(0);
    };
    process.on('SIGINT', shutdown);
    process.on('SIGTERM', shutdown);
}
main().catch(err => {
    process.stderr.write(`orca-example-hackernews: ${err instanceof Error ? err.message : err}\n`);
    process.exit(1);
});
