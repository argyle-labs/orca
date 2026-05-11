import type { OrcaClient } from '$lib/orca/index';
import { orca } from '$lib/orcaClient';
import { notifications } from '$lib/stores/notifications';

/**
 * Keys on `OrcaClient` whose values are async tool methods of the shape
 * `(args: X) => Promise<Y>`. Excludes `free`, `Symbol.dispose`, and any
 * future non-tool members automatically.
 */
export type ToolName = {
  [K in keyof OrcaClient]: OrcaClient[K] extends (args: never) => Promise<unknown> ? K : never;
}[keyof OrcaClient];

/** Argument type for a given tool — extracted from the generated declaration. */
export type ToolArgs<N extends ToolName> = OrcaClient[N] extends (args: infer A) => Promise<unknown>
  ? A
  : never;

/** Result type for a given tool. */
export type ToolResult<N extends ToolName> = OrcaClient[N] extends (args: never) => Promise<infer R>
  ? R
  : never;

/**
 * Invoke an OrcaTool by name with full type safety.
 *
 * - `name` is constrained to actual tool methods on `OrcaClient`.
 * - `args` is checked against the tool's declared input type.
 * - The return is the tool's declared output type (or `null` on caught failure).
 *
 * On failure, pushes an error toast unless `silent`. On success with a
 * `successMessage`, pushes a success toast.
 *
 * Every UI call goes through this — never hand-roll `fetch` or call WASM
 * methods directly in component bodies.
 */
export async function runTool<N extends ToolName>(
  name: N,
  args: ToolArgs<N>,
  opts: { silent?: boolean; successMessage?: string } = {},
): Promise<ToolResult<N> | null> {
  try {
    const result = await callTool(name, args);
    if (opts.successMessage) notifications.success(opts.successMessage);
    return result;
  } catch (e) {
    if (!opts.silent) notifications.error(formatError(name, e));
    return null;
  }
}

/** Strict variant that throws — for callers that want to manage their own error UX. */
export async function callTool<N extends ToolName>(
  name: N,
  args: ToolArgs<N>,
): Promise<ToolResult<N>> {
  const client = await orca();
  // Narrowing through `keyof OrcaClient` is sound because `ToolName` is
  // derived from that exact set, and we've already type-checked the args.
  const method = client[name] as unknown as (a: ToolArgs<N>) => Promise<ToolResult<N>>;
  return method.call(client, args);
}

function formatError(toolName: string, err: unknown): string {
  const msg = err instanceof Error ? err.message : String(err);
  return `${toolName}: ${msg}`;
}
