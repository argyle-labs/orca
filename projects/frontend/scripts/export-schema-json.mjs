#!/usr/bin/env node
/**
 * Exports the ice-age schema visualizer data as static JSON.
 *
 * Reads:  DB_REPO_ROOT/generated/types.d.ts + database-constraints.json
 *         ~/code/my-project/scripts/db/schemaVisualizerConfig.ts
 *
 * Writes: ~/code/my-project/docs/schema/schema.json  (for MCP)
 *
 * Environment:
 *   DB_REPO_ROOT   Path to the database repo root (default: ~/code/my-project/db)
 *   ICE_AGE_BIN    Path to ice-age dist dir (default: node_modules/@myorg/ice-age/dist)
 *
 * Prerequisites: run scripts/db/generate-schema.py first.
 *
 * Usage:
 *   node site/scripts/export-schema-json.mjs
 */

import { writeFileSync, mkdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const HOME = process.env.HOME ?? '';

const ICE_AGE_BIN = process.env.ICE_AGE_BIN
  ?? join(HOME, 'code/my-project/node_modules/@myorg/ice-age/dist');

const { generateTabData } =
  await import(`${ICE_AGE_BIN}/visualizer/schemaVisualizer.js`);
const { loadVisualizerConfig } =
  await import(`${ICE_AGE_BIN}/visualizer/configLoader.js`);

const configPath = join(__dirname, '../../scripts/db/schemaVisualizerConfig.ts');
const outDir = join(__dirname, '../../docs/schema');
const outPath = join(outDir, 'schema.json');

const tabs = await loadVisualizerConfig(configPath);
if (!tabs.length) {
  console.error('No config loaded — run scripts/db/generate-schema.py first.');
  process.exit(1);
}

const data = tabs.map(({ config }) => generateTabData(config));

mkdirSync(outDir, { recursive: true });
writeFileSync(outPath, JSON.stringify({ tabs: data, generatedAt: new Date().toISOString() }, null, 2));

const tableCount = data.reduce((n, t) => n + t.tables.length, 0);
const fkCount = data.reduce((n, t) => n + t.fks.length, 0);
console.log(`Wrote ${outPath}`);
console.log(`  ${tableCount} tables, ${fkCount} FK relationships, ${data.length} tab(s)`);
