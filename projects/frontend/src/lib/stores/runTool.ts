// Tool dispatch — designed so the bulk of the SDK can be tree-shaken away
// AND every callsite is fully typed end-to-end (no `any`, no widening).
//
// Two paths:
//
// 1. **Static dispatch (preferred, critical-path).** Import the specific
//    `sdk.gen` function by NAME, invoke it directly with its typed
//    options, and hand the resulting promise to `unwrap()`. Only that one
//    SDK function and its types are reachable from this file's chunk, so
//    Rolldown drops the other ~134 SDK functions on build.
//
//    ```ts
//    import { configGet } from '$lib/client/sdk.gen';
//    import { unwrap, peerHeader } from '$lib/stores/runTool';
//    const r = await unwrap(configGet({ body: { noun, name } }));
//    // peer-dispatch:
//    const probe = await unwrap(systemUpdate({ body: {}, headers: peerHeader(peerId) }));
//    ```
//
// 2. **Dynamic dispatch (CommandPalette, generic enumeration).** Call
//    `callTool(name, args, opts)` — it lazy-imports `sdk.gen` on first use
//    (chunked separately by Vite/Rolldown), so the SDK cost stays OFF the
//    first-paint critical path. Typing degrades to `unknown` because the
//    tool name isn't known until runtime — narrow at the callsite.
//
// HARD RULE: do NOT `import * as sdk from '$lib/client/sdk.gen'` from this
// file or any caller of `unwrap`. That re-introduces the barrel and
// defeats tree-shaking. The dynamic `import()` below is the ONLY allowed
// barrel — and it's behind a Promise so the SDK doesn't pin to the
// initial chunk.

import { notifications } from '$lib/stores/notifications';

/** Hey-api's uniform result envelope. */
export type ToolResult<T> = {
  data?: T;
  error?: unknown;
  response?: Response;
};

/**
 * Narrow a hey-api result envelope to its `data` payload. The global
 * client config sets `throwOnError: true`, so by the time we get here a
 * non-2xx has already thrown — this helper is just a typed `.data` pick.
 */
export async function unwrap<T>(promise: Promise<ToolResult<T>>): Promise<T> {
  const res = await promise;
  return res.data as T;
}

/**
 * Build the `X-Orca-Peer` header for mesh-proxied calls. Returns
 * `undefined` for the synthetic "local" peer so loopback calls don't
 * bounce through the mesh. Spread into `headers` at the callsite.
 */
export function peerHeader(peer?: string | null): { 'X-Orca-Peer': string } | undefined {
  return peer && peer !== 'local' ? { 'X-Orca-Peer': peer } : undefined;
}

// ── Dynamic-dispatch path ───────────────────────────────────────────────
//
// Lazy SDK loader — first call triggers the chunked import; subsequent
// calls reuse the resolved module. Off the first-paint critical path.

type DynamicFn = (opts?: {
  body?: unknown;
  headers?: Record<string, string>;
}) => Promise<ToolResult<unknown>>;

type SdkModule = Record<string, DynamicFn>;
let sdkPromise: Promise<SdkModule> | null = null;
function loadSdk(): Promise<SdkModule> {
  if (!sdkPromise) {
    sdkPromise = import('$lib/client/sdk.gen') as unknown as Promise<SdkModule>;
  }
  return sdkPromise;
}

export type ToolName = string;
export type DispatchOpts = { peer?: string | null };

/**
 * Dynamic-dispatch helper for callers that don't know the tool name at
 * compile time (palette, generic enumeration). Lazy-imports the SDK on
 * first call. Prefer the static `unwrap(sdkFn(opts))` path everywhere
 * else — only that path is tree-shakeable.
 */
export async function callTool<T = unknown>(
  name: ToolName,
  args: Record<string, unknown> = {},
  opts: DispatchOpts = {},
): Promise<T> {
  const sdk = await loadSdk();
  const fn = sdk[name];
  if (typeof fn !== 'function') {
    throw new Error(`unknown tool: ${name}`);
  }
  // hey-api emits one of `.get(/.head(/.post(/.put(/.delete(/.patch(` in
  // the generated function body. GETs reject any body; POSTs need one.
  const wantsBody = !/\.(get|head)\(/.test(fn.toString());
  const headers = peerHeader(opts.peer);
  const callOpts: { body?: unknown; headers?: Record<string, string> } = {};
  if (wantsBody) callOpts.body = args;
  if (headers) callOpts.headers = headers;
  return (await unwrap(fn(callOpts) as Promise<ToolResult<T>>)) as T;
}

/**
 * Toast-on-error variant for fire-and-forget UI calls (palette, etc).
 */
export async function runTool(
  name: ToolName,
  args: Record<string, unknown> = {},
  opts: { silent?: boolean; successMessage?: string } = {},
): Promise<unknown> {
  try {
    const result = await callTool(name, args);
    if (opts.successMessage) notifications.success(opts.successMessage);
    return result;
  } catch (e) {
    if (!opts.silent) notifications.error(`${name}: ${e instanceof Error ? e.message : String(e)}`);
    return null;
  }
}

/** Names of every callable tool — used by the command palette. Async + lazy. */
export async function allToolNames(): Promise<ToolName[]> {
  const sdk = await loadSdk();
  return Object.keys(sdk)
    .filter(k => typeof sdk[k] === 'function')
    .sort();
}
