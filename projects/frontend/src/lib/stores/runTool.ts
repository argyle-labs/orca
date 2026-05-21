// Generic tool dispatcher used by the command palette + any caller that
// wants runtime tool-name lookup with a uniform toast/error pipeline.
//
// The hey-api SDK already produces a typed function per OpenAPI operation —
// when you know the tool at compile time, import it directly from
// `$lib/client/sdk.gen` and call it as a normal function. This module exists
// for the dynamic case (palette enumeration, generic dispatch).

import * as sdk from '$lib/client/sdk.gen';
import { notifications } from '$lib/stores/notifications';

/**
 * Every exported async function in the generated SDK is callable here.
 * Names are operationIds: `health`, `podList`, `hostInfo`, etc.
 */
export type ToolName = keyof typeof sdk;

/**
 * Hey-api functions accept an options object; we pass `body` for POSTs and
 * an empty object for GETs. The dispatcher hides the difference.
 */
type AnyToolFn = (opts?: { body?: unknown }) => Promise<{
  data?: unknown;
  error?: unknown;
  response?: Response;
}>;

/**
 * Invoke a tool by operationId. Pushes an error toast on failure unless
 * `silent`. Returns the unwrapped `data` field, or null if the call errored.
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
    if (!opts.silent) notifications.error(formatError(name, e));
    return null;
  }
}

/**
 * Strict variant — throws on failure. Use when the caller wants to manage
 * error UX itself.
 */
export async function callTool<T = unknown>(
  name: ToolName,
  args: Record<string, unknown> = {},
): Promise<T> {
  const fn = (sdk as unknown as Record<string, AnyToolFn>)[name as string];
  if (typeof fn !== 'function') {
    throw new Error(`unknown tool: ${String(name)}`);
  }
  // hey-api emits one of `.get(/`.post(`/`.put(`/`.delete(`/`.patch(` in
  // the body of every generated SDK function. GETs reject any body (fetch
  // refuses), POSTs need a body so the request gets a Content-Type and
  // doesn't 415. Detect by inspecting the compiled source.
  const wantsBody = !/\.(get|head)\(/.test(fn.toString());
  const res = await fn(wantsBody ? { body: args } : undefined);
  if (res.error || !res.response?.ok) {
    const msg =
      (res.error as { error?: string } | undefined)?.error ??
      `${String(name)} failed (${res.response?.status ?? 'no response'})`;
    throw new Error(msg);
  }
  return res.data as T;
}

/** Names of every callable tool — used by the command palette. */
export function allToolNames(): ToolName[] {
  return (Object.keys(sdk) as ToolName[])
    .filter(k => typeof (sdk as Record<string, unknown>)[k as string] === 'function')
    .sort();
}

function formatError(toolName: string, err: unknown): string {
  const msg = err instanceof Error ? err.message : String(err);
  return `${toolName}: ${msg}`;
}
