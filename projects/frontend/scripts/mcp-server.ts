#!/usr/bin/env node
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { readdirSync, readFileSync, existsSync, statSync } from 'fs';
import { join, relative, extname, basename } from 'path';
import { z } from 'zod';

const HOME = process.env.HOME ?? '';

const ROOTS: Record<string, string> = {
  project: process.env.PROJECT_ROOT ?? join(HOME, 'code', 'my-project'),
  orca: process.env.ORCA_ROOT ?? join(HOME, '.brain'),
};

const ROOT_IGNORED: Record<string, Set<string>> = {
  project: new Set(['node_modules', '.git', '.next', 'dist', 'build', 'vendor', 'www', 'docs']),
  orca: new Set(['.git', 'logs', 'memory', 'notes', 'plans', 'plugins', '.trash', 'node_modules']),
};

const MD_EXTS = new Set(['.md', '.mdx']);

interface TreeNode {
  name: string;
  path: string;
  type: 'file' | 'dir';
  children?: TreeNode[];
}

function isDir(full: string, entry: { isDirectory(): boolean; isSymbolicLink(): boolean }): boolean {
  if (entry.isDirectory()) return true;
  if (entry.isSymbolicLink()) {
    try { return statSync(full).isDirectory(); } catch { return false; }
  }
  return false;
}

function buildTree(dir: string, rootDir: string, ignored: Set<string>): TreeNode[] {
  let entries: ReturnType<typeof readdirSync>;
  try { entries = readdirSync(dir, { withFileTypes: true }); }
  catch { return []; }
  const nodes: TreeNode[] = [];
  for (const entry of entries) {
    if (ignored.has(entry.name) || entry.name.startsWith('.')) continue;
    const full = join(dir, entry.name);
    const rel = relative(rootDir, full);
    if (isDir(full, entry)) {
      const children = buildTree(full, rootDir, ignored);
      if (children.length > 0) nodes.push({ name: entry.name, path: rel, type: 'dir', children });
    } else if (MD_EXTS.has(extname(entry.name))) {
      nodes.push({ name: basename(entry.name, extname(entry.name)), path: rel, type: 'file' });
    }
  }
  return nodes.sort((a, b) => {
    if (a.type !== b.type) return a.type === 'dir' ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

function countFiles(nodes: TreeNode[]): number {
  return nodes.reduce((n, node) => n + (node.type === 'file' ? 1 : countFiles(node.children ?? [])), 0);
}

function findSingleFile(nodes: TreeNode[]): TreeNode | null {
  for (const node of nodes) {
    if (node.type === 'file') return node;
    const found = findSingleFile(node.children ?? []);
    if (found) return found;
  }
  return null;
}

function compactTree(nodes: TreeNode[]): TreeNode[] {
  const result: TreeNode[] = [];
  for (const node of nodes) {
    if (node.type === 'file') { result.push(node); continue; }
    const children = compactTree(node.children ?? []);
    if (countFiles(children) === 1) { result.push(findSingleFile(children)!); continue; }
    if (children.length === 1 && children[0].type === 'dir') {
      result.push({ ...children[0], name: `${node.name}/${children[0].name}` });
      continue;
    }
    result.push({ ...node, children });
  }
  return result;
}

function collectAllFiles(nodes: TreeNode[]): TreeNode[] {
  const files: TreeNode[] = [];
  for (const node of nodes) {
    if (node.type === 'file') files.push(node);
    else files.push(...collectAllFiles(node.children ?? []));
  }
  return files;
}

function resolveFile(rootName: string, docPath: string): string | null {
  const rootDir = ROOTS[rootName];
  if (!rootDir) return null;
  for (const ext of ['.md', '.mdx', '']) {
    try {
      const full = join(rootDir, docPath + ext);
      if (existsSync(full) && statSync(full).isFile()) return full;
    } catch {}
  }
  return null;
}

const mcp = new McpServer({ name: 'orca', version: '0.3.0' });

mcp.tool(
  'list_roots',
  'List all available documentation roots (project, orca)',
  {},
  async () => {
    const entries = Object.entries(ROOTS).map(([name, path]) => ({
      root: name,
      path,
      exists: existsSync(path),
      docs: (() => {
        try { return countFiles(buildTree(path, path, ROOT_IGNORED[name] ?? new Set())); }
        catch { return 0; }
      })(),
    }));
    return { content: [{ type: 'text', text: JSON.stringify(entries, null, 2) }] };
  },
);

mcp.tool(
  'get_tree',
  'Get the compacted documentation tree for a root, optionally scoped to a subpath',
  {
    root: z.string().describe('Root name: project | orca'),
    path: z.string().optional().describe('Subpath within root (e.g. "admin-api" or "ai/claude/agents")'),
  },
  async ({ root: rootName, path: subPath }) => {
    const rootDir = ROOTS[rootName];
    if (!rootDir) return { content: [{ type: 'text', text: `Unknown root: ${rootName}` }], isError: true };
    const ignored = ROOT_IGNORED[rootName] ?? new Set();
    const dir = subPath ? join(rootDir, subPath) : rootDir;
    const raw = buildTree(dir, rootDir, ignored);
    return { content: [{ type: 'text', text: JSON.stringify(compactTree(raw), null, 2) }] };
  },
);

mcp.tool(
  'read_doc',
  'Read a documentation file by root and path (e.g. root=project, path=admin-api/README)',
  {
    root: z.string().describe('Root name: project | orca'),
    path: z.string().describe('Path relative to root, without extension'),
  },
  async ({ root: rootName, path: docPath }) => {
    const rootDir = ROOTS[rootName];
    if (!rootDir) return { content: [{ type: 'text', text: `Unknown root: ${rootName}` }], isError: true };
    const full = resolveFile(rootName, docPath);
    if (!full || !full.startsWith(rootDir)) {
      return { content: [{ type: 'text', text: `Not found: ${rootName}/${docPath}` }], isError: true };
    }
    return { content: [{ type: 'text', text: readFileSync(full, 'utf-8') }] };
  },
);

mcp.tool(
  'search',
  'Search documentation for a keyword across one or all roots',
  {
    query: z.string().describe('Search term (case-insensitive)'),
    root: z.string().optional().describe('Limit to root: project | orca | all (default: all)'),
  },
  async ({ query, root: rootName = 'all' }) => {
    const targets = rootName === 'all'
      ? Object.entries(ROOTS)
      : Object.entries(ROOTS).filter(([name]) => name === rootName);

    const pattern = new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
    const results: { path: string; matches: string[] }[] = [];

    for (const [name, rootDir] of targets) {
      const ignored = ROOT_IGNORED[name] ?? new Set();
      const files = collectAllFiles(buildTree(rootDir, rootDir, ignored));
      for (const file of files) {
        const full = join(rootDir, file.path);
        try {
          const lines = readFileSync(full, 'utf-8').split('\n');
          const matches = lines
            .map((line, i) => ({ line, i }))
            .filter(({ line }) => { pattern.lastIndex = 0; return pattern.test(line); })
            .map(({ line, i }) => `L${i + 1}: ${line.trim()}`);
          if (matches.length > 0) results.push({ path: `${name}/${file.path}`, matches: matches.slice(0, 5) });
        } catch {}
      }
    }

    const text = results.length === 0
      ? `No results for "${query}"`
      : results.map((r) => `## ${r.path}\n${r.matches.join('\n')}`).join('\n\n');

    return { content: [{ type: 'text', text }] };
  },
);

mcp.tool(
  'list_agents',
  'List all Claude agent definitions from the orca vault',
  {},
  async () => {
    const agentsDir = join(ROOTS.orca, 'ai', 'claude', 'agents');
    if (!existsSync(agentsDir)) return { content: [{ type: 'text', text: 'Agents dir not found' }], isError: true };
    try {
      const files = readdirSync(agentsDir).filter((f) => f.endsWith('.md'));
      return { content: [{ type: 'text', text: files.map((f) => f.replace('.md', '')).join('\n') }] };
    } catch { return { content: [{ type: 'text', text: 'Error reading agents dir' }], isError: true }; }
  },
);

mcp.tool(
  'list_commands',
  'List all Claude slash commands and skills from the orca vault',
  {},
  async () => {
    const cmdsDir = join(ROOTS.orca, 'ai', 'claude', 'commands');
    if (!existsSync(cmdsDir)) return { content: [{ type: 'text', text: 'Commands dir not found' }], isError: true };
    try {
      const files = readdirSync(cmdsDir).filter((f) => f.endsWith('.md'));
      return { content: [{ type: 'text', text: files.map((f) => '/' + f.replace('.md', '')).join('\n') }] };
    } catch { return { content: [{ type: 'text', text: 'Error reading commands dir' }], isError: true }; }
  },
);

const transport = new StdioServerTransport();
await mcp.connect(transport);
