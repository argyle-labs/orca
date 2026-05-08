#!/usr/bin/env tsx
/**
 * orca codegen — generates TypeScript types, fetch functions, and TanStack Query hooks
 * from the orca backend's OpenAPI 3.1 spec at /api/openapi.json.
 *
 * Run: npx tsx scripts/gen.ts [--url http://localhost:12000] [--out src/api]
 * Or:  orca gen
 */

import { writeFileSync, mkdirSync } from 'fs';
import { join } from 'path';

// ── CLI args ──────────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
const get = (flag: string, def: string) => {
  const i = args.indexOf(flag);
  return i !== -1 && args[i + 1] ? args[i + 1] : def;
};

const BASE_URL = get('--url', 'http://localhost:12000');
const OUT_DIR = get('--out', 'src/lib/api');
const SPEC_FILE = get('--file', '');

// ── OpenAPI types (minimal subset we need) ────────────────────────────────────

interface OpenApiSpec {
  openapi: string;
  info: { title: string; version: string };
  paths: Record<string, PathItem>;
  components?: { schemas?: Record<string, SchemaObject> };
}

interface PathItem {
  get?: Operation;
  post?: Operation;
  put?: Operation;
  patch?: Operation;
  delete?: Operation;
}

interface Operation {
  operationId?: string;
  summary?: string;
  description?: string;
  tags?: string[];
  parameters?: Parameter[];
  requestBody?: { content: Record<string, { schema: SchemaObject }> };
  responses: Record<string, { description: string; content?: Record<string, { schema: SchemaObject }> }>;
}

interface Parameter {
  name: string;
  in: 'query' | 'path' | 'header';
  required?: boolean;
  schema: SchemaObject;
  description?: string;
}

interface SchemaObject {
  // OpenAPI 3.1 allows type to be an array (e.g. ["string", "null"]) for nullable types.
  type?: string | string[];
  format?: string;
  description?: string;
  properties?: Record<string, SchemaObject>;
  items?: SchemaObject;
  required?: string[];
  $ref?: string;
  allOf?: SchemaObject[];
  oneOf?: SchemaObject[];
  nullable?: boolean;
  enum?: string[];
  additionalProperties?: SchemaObject | boolean;
}

// Normalize OpenAPI 3.1 nullable encodings into the 3.0-style nullable + scalar type
// our generator already understands. Returns { schema, nullable } with `schema` having
// a single string `type` and `null` variants stripped from `oneOf`.
function normalizeNullable(schema: SchemaObject): { schema: SchemaObject; nullable: boolean } {
  let nullable = !!schema.nullable;
  let next: SchemaObject = schema;

  // 3.1 array form: type: ["string", "null"] → type: "string", nullable
  if (Array.isArray(schema.type)) {
    const types = schema.type.filter((t) => t !== 'null');
    if (schema.type.includes('null')) nullable = true;
    next = { ...schema, type: types[0] };
  }

  // 3.1 oneOf form: oneOf: [{type:"null"}, {$ref}] → strip null, mark nullable
  if (next.oneOf) {
    const nonNull = next.oneOf.filter((s) => !(s.type === 'null' || (Array.isArray(s.type) && s.type.length === 1 && s.type[0] === 'null')));
    if (nonNull.length !== next.oneOf.length) nullable = true;
    if (nonNull.length === 1) {
      next = { ...nonNull[0] };
    } else if (nonNull.length !== next.oneOf.length) {
      next = { ...next, oneOf: nonNull };
    }
  }

  return { schema: next, nullable };
}

// ── Fetch spec ────────────────────────────────────────────────────────────────

let spec: OpenApiSpec;
if (SPEC_FILE) {
  const { readFileSync } = await import('fs');
  spec = JSON.parse(readFileSync(SPEC_FILE, 'utf-8'));
} else {
  const res = await fetch(`${BASE_URL}/api/openapi.json`);
  if (!res.ok) {
    console.error(`Failed to fetch spec: ${res.status} ${res.statusText}`);
    process.exit(1);
  }
  spec = await res.json();
}

mkdirSync(OUT_DIR, { recursive: true });

// ── Schema → TypeScript type ──────────────────────────────────────────────────

// ns: optional namespace prefix (e.g. 'T') used when generating client/hooks files
function schemaToTs(schema: SchemaObject, indent = 0, inline = false, ns = ''): string {
  if (!schema) return 'unknown';

  // Collapse 3.1 nullable encodings before pattern-matching on shape.
  const norm = normalizeNullable(schema);
  if (norm.nullable) {
    const inner = schemaToTs({ ...norm.schema, nullable: false }, indent, true, ns);
    return `${inner} | null`;
  }
  schema = norm.schema;

  if (schema.$ref) {
    // utoipa may emit super.TypeName or crate.TypeName for types referenced
    // from a parent module — strip module path prefixes, keep only the type name.
    const raw = schema.$ref.split('/').pop()!;
    const name = raw.includes('.') ? raw.split('.').pop()! : raw;
    return ns ? `${ns}.${name}` : name;
  }

  if (schema.allOf) {
    return schema.allOf.map((s) => schemaToTs(s, indent, true, ns)).join(' & ');
  }

  if (schema.oneOf) {
    return schema.oneOf.map((s) => schemaToTs(s, indent, true, ns)).join(' | ');
  }

  if (schema.enum) {
    return schema.enum.map((v) => JSON.stringify(v)).join(' | ');
  }

  const pad = '  '.repeat(indent);
  const inner = '  '.repeat(indent + 1);

  switch (schema.type) {
    case 'string':
      return schema.format === 'date-time' ? 'string' : 'string';
    case 'integer':
    case 'number':
      return 'number';
    case 'boolean':
      return 'boolean';
    case 'array':
      return schema.items ? `${schemaToTs(schema.items, indent, true, ns)}[]` : 'unknown[]';
    case 'object': {
      if (schema.additionalProperties && typeof schema.additionalProperties === 'object') {
        return `Record<string, ${schemaToTs(schema.additionalProperties, indent, true, ns)}>`;
      }
      if (!schema.properties) return 'Record<string, unknown>';
      const props = Object.entries(schema.properties).map(([key, val]) => {
        const optional = !schema.required?.includes(key) ? '?' : '';
        const comment = val.description ? `\n${inner}/** ${val.description} */\n${inner}` : `\n${inner}`;
        // schemaToTs handles its own nullability now (3.0 nullable + 3.1 type-array/oneOf-null).
        return `${comment}${key}${optional}: ${schemaToTs(val, indent + 1, true, ns)};`;
      });
      return `{${props.join('')}\n${pad}}`;
    }
    default:
      // Treat schema with properties but no type as object
      if (schema.properties) return schemaToTs({ ...schema, type: 'object' }, indent, inline, ns);
      return 'unknown';
  }
}

// ── Generate types.ts ─────────────────────────────────────────────────────────

function generateTypes(spec: OpenApiSpec): string {
  const schemas = spec.components?.schemas ?? {};
  const lines: string[] = [
    '// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.',
    '// Source: ' + BASE_URL + '/api/openapi.json',
    '',
  ];

  for (const [name, schema] of Object.entries(schemas)) {
    if (schema.description) {
      lines.push(`/** ${schema.description} */`);
    }
    if (schema.enum) {
      lines.push(`export type ${name} = ${schemaToTs(schema)};`);
    } else if (schema.type === 'object' || schema.properties) {
      lines.push(`export interface ${name} ${schemaToTs(schema)}`);
    } else {
      lines.push(`export type ${name} = ${schemaToTs(schema)};`);
    }
    lines.push('');
  }

  return lines.join('\n');
}

// ── Operation helpers ─────────────────────────────────────────────────────────

interface ParsedOp {
  operationId: string;
  method: string;
  path: string;
  summary?: string;
  tags: string[];
  pathParams: Parameter[];
  queryParams: Parameter[];
  requestBodyType: string | null;
  responseType: string;
  isGet: boolean;
}

function responseType(op: Operation, ns = ''): string {
  const ok = op.responses['200'];
  if (!ok?.content) return 'void';
  const jsonContent = ok.content['application/json'];
  if (!jsonContent) return 'void';
  return schemaToTs(jsonContent.schema, 0, true, ns);
}

function requestBodyType(op: Operation, ns = ''): string | null {
  const json = op.requestBody?.content?.['application/json'];
  if (!json) return null;
  return schemaToTs(json.schema, 0, true, ns);
}

function collectOps(spec: OpenApiSpec): ParsedOp[] {
  const ops: ParsedOp[] = [];
  for (const [path, item] of Object.entries(spec.paths)) {
    for (const method of ['get', 'post', 'put', 'patch', 'delete'] as const) {
      const op = item[method];
      if (!op) continue;
      const operationId = op.operationId ?? `${method}${path.replace(/[^a-zA-Z0-9]/g, '_')}`;
      ops.push({
        operationId,
        method,
        path,
        summary: op.summary ?? op.description,
        tags: op.tags ?? [],
        pathParams: (op.parameters ?? []).filter((p) => p.in === 'path'),
        queryParams: (op.parameters ?? []).filter((p) => p.in === 'query'),
        requestBodyType: requestBodyType(op, 'T'),
        responseType: responseType(op, 'T'),
        isGet: method === 'get',
      });
    }
  }
  return ops;
}

// ── Generate client.ts ────────────────────────────────────────────────────────

function generateClient(_spec: OpenApiSpec, ops: ParsedOp[]): string {
  const lines: string[] = [
    '// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.',
    '',
    "import type * as T from './types';",
    '',
    `const BASE = '';  // same-origin — proxied via Vite in dev`,
    '',
    'async function request<R>(method: string, path: string, opts?: {',
    '  query?: Record<string, string | number | boolean | undefined>;',
    '  body?: unknown;',
    '}): Promise<R> {',
    '  const url = new URL(BASE + path, window.location.origin);',
    '  if (opts?.query) {',
    '    for (const [k, v] of Object.entries(opts.query)) {',
    '      if (v !== undefined) url.searchParams.set(k, String(v));',
    '    }',
    '  }',
    '  const res = await fetch(url.toString(), {',
    '    method,',
    '    headers: opts?.body ? { \'Content-Type\': \'application/json\' } : undefined,',
    '    body: opts?.body ? JSON.stringify(opts.body) : undefined,',
    '  });',
    '  if (!res.ok) {',
    '    const err = await res.json().catch(() => ({ error: res.statusText }));',
    '    throw new Error(err.error ?? `HTTP ${res.status}`);',
    '  }',
    '  const ct = res.headers.get(\'content-type\') ?? \'\';',
    '  if (ct.includes(\'application/json\')) return res.json();',
    '  return res.text() as unknown as R;',
    '}',
    '',
  ];

  for (const op of ops) {
    const hasPath = op.pathParams.length > 0;
    const hasQuery = op.queryParams.length > 0;
    const hasBody = op.requestBodyType !== null;

    // Build params interface — path params first (required), then query, then body
    const paramLines: string[] = [];
    for (const p of op.pathParams) {
      const doc = p.description ? ` // ${p.description}` : '';
      paramLines.push(`  ${p.name}: ${schemaToTs(p.schema, 0, true, 'T')};${doc}`);
    }
    if (hasQuery) {
      for (const p of op.queryParams) {
        const opt = p.required ? '' : '?';
        const doc = p.description ? ` // ${p.description}` : '';
        paramLines.push(`  ${p.name}${opt}: ${schemaToTs(p.schema, 0, true, 'T')};${doc}`);
      }
    }
    if (hasBody) {
      paramLines.push(`  body: ${op.requestBodyType};`);
    }

    const hasParams = paramLines.length > 0;
    const paramsArg = hasParams ? `params: {\n${paramLines.join('\n')}\n}` : '';
    const returnType = op.responseType === 'void' ? 'void' : op.responseType;

    if (op.summary) lines.push(`/** ${op.summary} */`);

    // Replace {param} placeholders in the path with template literal interpolations
    const resolvedPath = hasPath
      ? '`' + op.path.replace(/\{(\w+)\}/g, '${params.$1}') + '`'
      : `'${op.path}'`;

    const queryObj = hasQuery
      ? `{ ${op.queryParams.map((p) => p.name).join(', ')} }`
      : 'undefined';

    // Destructure query params (and body if coexisting with query) from params
    const destructureNames = [
      ...op.queryParams.map((p) => p.name),
      ...(hasBody && hasQuery ? ['body'] : []),
    ];
    const destructure = destructureNames.length > 0
      ? `  const { ${destructureNames.join(', ')} } = params;\n`
      : '';

    lines.push(
      `export async function ${op.operationId}(${paramsArg}): Promise<${returnType}> {`,
      destructure || '',
      `  return request<${returnType}>('${op.method.toUpperCase()}', ${resolvedPath}, {`,
      hasQuery ? `    query: ${queryObj},` : '',
      hasBody ? `    body: ${hasQuery ? 'body' : 'params.body'},` : '',
      '  });',
      '}',
      '',
    );
  }

  return lines.filter((l) => l !== '').join('\n');
}

// ── Stale time by domain tag ──────────────────────────────────────────────────
// Tags not listed here use the global QueryClient default (set in main.tsx).
// Order matters: first matching tag wins.

const STALE_BY_TAG: Record<string, number> = {
  docs:    5 * 60_000,  // vault docs — stable within a session
  library: 5 * 60_000,  // context7 library docs — stable
  schema:  5 * 60_000,  // MySQL schema — changes only on migration
  specs:   5 * 60_000,  // OpenAPI spec files — manually maintained
};

function staleTimeForTags(tags: string[]): number | null {
  for (const tag of tags) {
    if (tag in STALE_BY_TAG) return STALE_BY_TAG[tag];
  }
  return null; // use global default
}

// ── Generate hooks.ts ─────────────────────────────────────────────────────────

function generateHooks(ops: ParsedOp[]): string {
  const lines: string[] = [
    '// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.',
    '',
    "import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';",
    "import type { UseQueryOptions, UseMutationOptions } from '@tanstack/react-query';",
    "import type * as T from './types';",
    "import * as client from './client';",
    "import { staleMs } from './stale';",
    '',
  ];

  for (const op of ops) {
    const hasPath = op.pathParams.length > 0;
    const hasQuery = op.queryParams.length > 0;
    const hasBody = op.requestBodyType !== null;
    const hasParams = hasPath || hasQuery || hasBody;
    const returnType = op.responseType === 'void' ? 'void' : op.responseType;

    const paramsType = hasParams
      ? `Parameters<typeof client.${op.operationId}>[0]`
      : 'void';

    if (op.isGet) {
      // useQuery hook
      const hookName = `use${op.operationId.charAt(0).toUpperCase()}${op.operationId.slice(1)}`;
      const keyExpr = hasParams
        ? `['${op.operationId}', params]`
        : `['${op.operationId}']`;
      const paramsArg = hasParams ? `params: ${paramsType}, ` : '';
      const callArg = hasParams ? 'params' : '';
      const enabledLine = hasParams && op.queryParams.some((p) => p.required)
        ? `\n    enabled: !!params,`
        : '';
      const staleTime = staleTimeForTags(op.tags);
      const staleTimeLine = staleTime !== null ? `\n    staleTime: staleMs(${staleTime}),` : '';

      lines.push(
        `export function ${hookName}(${paramsArg}options?: Omit<UseQueryOptions<${returnType}>, 'queryKey' | 'queryFn'>) {`,
        `  return useQuery({`,
        `    queryKey: ${keyExpr},`,
        `    queryFn: () => client.${op.operationId}(${callArg}),${staleTimeLine}`,
        `    ...options,${enabledLine}`,
        `  });`,
        `}`,
        '',
      );
    } else {
      // useMutation hook
      const hookName = `use${op.operationId.charAt(0).toUpperCase()}${op.operationId.slice(1)}`;
      const mutationType = hasParams ? paramsType : 'void';

      lines.push(
        `export function ${hookName}(options?: UseMutationOptions<${returnType}, Error, ${mutationType}>) {`,
        `  return useMutation({`,
        `    mutationFn: (${hasParams ? 'params' : '_'}: ${mutationType}) => client.${op.operationId}(${hasParams ? 'params' : ''}),`,
        `    ...options,`,
        `  });`,
        `}`,
        '',
      );
    }
  }

  return lines.join('\n');
}

// ── Generate index.ts ─────────────────────────────────────────────────────────

function generateIndex(): string {
  return [
    '// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.',
    '',
    "export * from './types';",
    "export * from './client';",
    "export * from './hooks';",
    '',
  ].join('\n');
}

// ── Write outputs ─────────────────────────────────────────────────────────────

const ops = collectOps(spec);

const files: [string, string][] = [
  ['types.ts', generateTypes(spec)],
  ['client.ts', generateClient(spec, ops)],
  ['hooks.ts', generateHooks(ops)],
  ['index.ts', generateIndex()],
];

for (const [name, content] of files) {
  const path = join(OUT_DIR, name);
  writeFileSync(path, content, 'utf-8');
  console.log(`  ✓ ${path}`);
}

console.log(`\n  ${ops.length} operations → ${files.length} files`);
