#!/usr/bin/env node
/**
 * Generates a self-contained static schema site from the ice-age visualizer.
 *
 * Reads:  DB_REPO_ROOT/generated/types.d.ts + database-constraints.json
 *         ~/code/my-project/scripts/db/schemaVisualizerConfig.ts
 *
 * Writes: ~/code/my-project/docs/schema/index.html  (commit this)
 *         ~/code/my-project/docs/schema/schema.json  (for MCP)
 *
 * Environment:
 *   DB_REPO_ROOT   Path to the database repo root (default: ~/code/my-project/db)
 *   ICE_AGE_BIN    Path to ice-age dist dir (default: node_modules/@myorg/ice-age/dist)
 *
 * Prerequisites: run scripts/db/generate-schema.py first.
 *
 * Usage:
 *   node site/scripts/build-schema.mjs
 */

import { writeFileSync, mkdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const HOME = process.env.HOME ?? '';

const ICE_AGE_BIN = process.env.ICE_AGE_BIN
  ?? join(HOME, 'code/my-project/node_modules/@myorg/ice-age/dist');

const { generateTabData, getSchemaCSS, getSchemaClientJS } =
  await import(`${ICE_AGE_BIN}/visualizer/schemaVisualizer.js`);
const { loadVisualizerConfig } =
  await import(`${ICE_AGE_BIN}/visualizer/configLoader.js`);

const configPath = join(__dirname, '../../scripts/db/schemaVisualizerConfig.ts');
const outDir = join(__dirname, '../../docs/schema');
const outHtml = join(outDir, 'index.html');
const outJson = join(outDir, 'schema.json');

const tabs = await loadVisualizerConfig(configPath);
if (!tabs.length) {
  console.error('No config loaded — run scripts/db/generate-schema.py first.');
  process.exit(1);
}

const tabData = tabs.map(({ config }) => generateTabData(config));
const css = getSchemaCSS();
const js = getSchemaClientJS();

const showTabs = tabData.length > 1;
const pageTitle = showTabs ? 'Schema Visualizer' : tabData[0].title;
const data = JSON.stringify({ tabs: tabData, showTabs });

const html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${pageTitle} - Database Schema</title>
<style>
${css}
</style>
<script id="schema-data" type="application/json">${data}</script>
</head>
<body>
<div id="app"></div>
<script type="module">
${js}
</script>
</body>
</html>`;

mkdirSync(outDir, { recursive: true });
writeFileSync(outHtml, html);
writeFileSync(outJson, JSON.stringify({ tabs: tabData, generatedAt: new Date().toISOString() }, null, 2));

const tableCount = tabData.reduce((n, t) => n + t.tables.length, 0);
const fkCount = tabData.reduce((n, t) => n + t.fks.length, 0);
console.log(`Wrote ${outHtml}`);
console.log(`Wrote ${outJson}`);
console.log(`  ${tableCount} tables, ${fkCount} FK relationships, ${tabData.length} tab(s)`);
