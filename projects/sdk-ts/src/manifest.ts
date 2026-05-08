/**
 * Plugin manifest — `orca-plugin.toml`. Wire-compatible with
 * projects/sdk/src/manifest.rs and projects/sdk-go/manifest.
 */
import { readFile } from 'node:fs/promises';
import { parse as parseToml } from 'smol-toml';

export const FILENAME = 'orca-plugin.toml';

export type Sensitivity = 'general' | 'sensitive';
export type RuntimeMode = 'process';

export interface PluginSection {
  id: string;
  version: string;
  min_orca_version: string;
}

export interface RuntimeSection {
  binary?: string;
  image?: string;
  mode: RuntimeMode;
  eager: boolean;
}

export interface SurfacesSection {
  mcp: boolean;
  cli: boolean;
  ui: boolean;
  docs: boolean;
  jobs: boolean;
  storage: boolean;
  federation: boolean;
}

export interface CapabilityDecl {
  name: string;
  sensitivity: Sensitivity;
}

export interface Manifest {
  plugin: PluginSection;
  runtime: RuntimeSection;
  surfaces: SurfacesSection;
  capabilities: CapabilityDecl[];
}

const ALLOWED_TOP = new Set(['plugin', 'runtime', 'surfaces', 'capabilities']);
const ALLOWED_PLUGIN = new Set(['id', 'version', 'min_orca_version']);
const ALLOWED_RUNTIME = new Set(['binary', 'image', 'mode', 'eager']);
const ALLOWED_SURFACES = new Set([
  'mcp',
  'cli',
  'ui',
  'docs',
  'jobs',
  'storage',
  'federation',
]);
const ALLOWED_CAPABILITY = new Set(['name', 'sensitivity']);

function denyUnknown(obj: Record<string, unknown>, allowed: Set<string>, where: string): void {
  for (const k of Object.keys(obj)) {
    if (!allowed.has(k)) throw new Error(`unknown field "${k}" in ${where}`);
  }
}

function checkSemver(v: string, field: string): void {
  if (v.includes('-') || v.includes('+')) {
    throw new Error(`${field} "${v}": pre-release/build metadata not supported in v0`);
  }
  const parts = v.split('.');
  if (parts.length === 0) throw new Error(`${field} "${v}": empty`);
  for (const p of parts) {
    if (p.length === 0) throw new Error(`${field} "${v}": empty component`);
    if (!/^[0-9]+$/.test(p)) throw new Error(`${field} "${v}": bad numeric component "${p}"`);
  }
}

/** Parse a manifest from a TOML string. */
export function parseString(s: string): Manifest {
  const raw = parseToml(s) as Record<string, unknown>;
  denyUnknown(raw, ALLOWED_TOP, 'top-level');

  const plugin = (raw['plugin'] ?? {}) as Record<string, unknown>;
  denyUnknown(plugin, ALLOWED_PLUGIN, '[plugin]');
  const runtime = (raw['runtime'] ?? {}) as Record<string, unknown>;
  denyUnknown(runtime, ALLOWED_RUNTIME, '[runtime]');
  const surfacesRaw = (raw['surfaces'] ?? {}) as Record<string, unknown>;
  denyUnknown(surfacesRaw, ALLOWED_SURFACES, '[surfaces]');
  const capsRaw = (raw['capabilities'] ?? []) as Array<Record<string, unknown>>;
  for (const c of capsRaw) denyUnknown(c, ALLOWED_CAPABILITY, '[[capabilities]]');

  const id = String(plugin['id'] ?? '').trim();
  if (!id) throw new Error('plugin.id must not be empty');
  for (const ch of id) {
    if (ch === ' ' || ch === '\t' || ch === '\n' || ch === '/' || ch === '\\') {
      throw new Error(
        `plugin.id "${id}" contains invalid characters (whitespace or path separators)`,
      );
    }
  }
  const version = String(plugin['version'] ?? '');
  const minOrca = String(plugin['min_orca_version'] ?? '');
  checkSemver(version, 'plugin.version');
  checkSemver(minOrca, 'plugin.min_orca_version');

  const binary = runtime['binary'] === undefined ? undefined : String(runtime['binary']);
  const image = runtime['image'] === undefined ? undefined : String(runtime['image']);
  if (binary !== undefined && image !== undefined) {
    throw new Error('runtime.binary and runtime.image are mutually exclusive');
  }
  if (binary === undefined && image === undefined) {
    throw new Error('runtime requires either `binary` or `image`');
  }
  const mode = (runtime['mode'] ?? 'process') as string;
  if (mode !== 'process') {
    throw new Error(`runtime.mode "${mode}": only "process" is supported in v0`);
  }
  const eager = Boolean(runtime['eager'] ?? false);

  const surfaces: SurfacesSection = {
    mcp: Boolean(surfacesRaw['mcp'] ?? false),
    cli: Boolean(surfacesRaw['cli'] ?? false),
    ui: Boolean(surfacesRaw['ui'] ?? false),
    docs: Boolean(surfacesRaw['docs'] ?? false),
    jobs: Boolean(surfacesRaw['jobs'] ?? false),
    storage: Boolean(surfacesRaw['storage'] ?? false),
    federation: Boolean(surfacesRaw['federation'] ?? false),
  };

  const capabilities: CapabilityDecl[] = [];
  const seen = new Set<string>();
  for (const c of capsRaw) {
    const name = String(c['name'] ?? '').trim();
    if (!name) throw new Error('capability.name must not be empty');
    if (seen.has(name)) throw new Error(`duplicate capability "${name}"`);
    seen.add(name);
    const sensitivity = String(c['sensitivity'] ?? '');
    if (sensitivity !== 'general' && sensitivity !== 'sensitive') {
      throw new Error(
        `capability "${name}": sensitivity must be "general" or "sensitive", got "${sensitivity}"`,
      );
    }
    capabilities.push({ name, sensitivity });
  }

  const result: Manifest = {
    plugin: { id, version, min_orca_version: minOrca },
    runtime: { mode: 'process', eager, ...(binary !== undefined ? { binary } : {}), ...(image !== undefined ? { image } : {}) },
    surfaces,
    capabilities,
  };
  return result;
}

/** Parse a manifest from a file path. */
export async function parseFile(path: string): Promise<Manifest> {
  const data = await readFile(path, 'utf8');
  return parseString(data);
}
