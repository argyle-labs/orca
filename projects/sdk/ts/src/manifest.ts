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

/**
 * `[[depends_on]]` — peer plugin this plugin needs at runtime. The host
 * enforces presence of required deps before the plugin's tools are
 * dispatchable; optional deps degrade rather than reject.
 */
export interface PluginDependency {
  id: string;
  min_version: string;
  /** Default false — required. */
  optional?: boolean;
}

export interface Manifest {
  plugin: PluginSection;
  runtime: RuntimeSection;
  surfaces: SurfacesSection;
  capabilities: CapabilityDecl[];
  depends_on: PluginDependency[];
}

const ALLOWED_TOP = new Set(['plugin', 'runtime', 'surfaces', 'capabilities', 'depends_on']);
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
const ALLOWED_DEPEND = new Set(['id', 'min_version', 'optional']);

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
  const depsRaw = (raw['depends_on'] ?? []) as Array<Record<string, unknown>>;
  for (const d of depsRaw) denyUnknown(d, ALLOWED_DEPEND, '[[depends_on]]');

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

  const dependsOn: PluginDependency[] = [];
  const depIDs = new Set<string>();
  for (const d of depsRaw) {
    const depID = String(d['id'] ?? '').trim();
    if (!depID) throw new Error('depends_on.id must not be empty');
    if (depID === id) throw new Error(`plugin "${id}" cannot depend on itself`);
    if (depIDs.has(depID)) throw new Error(`duplicate dependency on "${depID}"`);
    depIDs.add(depID);
    const minVersion = String(d['min_version'] ?? '');
    checkSemver(minVersion, `depends_on[${depID}].min_version`);
    const dep: PluginDependency = { id: depID, min_version: minVersion };
    if (d['optional'] !== undefined) dep.optional = Boolean(d['optional']);
    dependsOn.push(dep);
  }

  const result: Manifest = {
    plugin: { id, version, min_orca_version: minOrca },
    runtime: { mode: 'process', eager, ...(binary !== undefined ? { binary } : {}), ...(image !== undefined ? { image } : {}) },
    surfaces,
    capabilities,
    depends_on: dependsOn,
  };
  return result;
}

/**
 * Format a {@link PluginDependency} for HelloOptions.pluginsRequired /
 * pluginsOptional — `<id>>=<min>`.
 */
export function formatDep(dep: PluginDependency): string {
  if (!dep.min_version) return dep.id;
  return `${dep.id}>=${dep.min_version}`;
}

/** Required deps formatted for HelloOptions.pluginsRequired. */
export function requiredDeps(m: Manifest): string[] {
  return m.depends_on.filter(d => !d.optional).map(formatDep);
}

/** Optional deps formatted for HelloOptions.pluginsOptional. */
export function optionalDeps(m: Manifest): string[] {
  return m.depends_on.filter(d => d.optional === true).map(formatDep);
}

/** Parse a manifest from a file path. */
export async function parseFile(path: string): Promise<Manifest> {
  const data = await readFile(path, 'utf8');
  return parseString(data);
}
