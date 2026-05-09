/**
 * Tools surface — plugins declare callable tools, host invokes them.
 * Wire-compatible with projects/sdk/src/tools.rs and projects/sdk-go/tools.
 *
 * Plugins that opt into `surfaces.mcp = true` declare tools via
 * orca/tools.declare; the host then dispatches orca/tools.call requests
 * back to the plugin over the same TCP+mTLS connection.
 *
 * Plugins declare bare names like "stack.list"; the host owns the
 * namespace and registers them as "<plugin_id>.stack.list".
 */
import type { Sensitivity } from './transport.js';
/** Method name plugins use to send their tool catalog. */
export declare const TOOLS_DECLARE_METHOD = "orca/tools.declare";
/** Method name the host uses to invoke a registered tool. */
export declare const TOOLS_CALL_METHOD = "orca/tools.call";
/**
 * Method name for plugin → host cross-plugin invocation. Caller supplies
 * a fully-qualified tool name `<plugin>.<tool>`; the host resolves the
 * owning peer via its in-process registry and forwards `tools.call`.
 */
export declare const TOOLS_INVOKE_METHOD = "orca/tools.invoke";
/**
 * Method name for plugin → host peer enumeration. Returns the currently
 * connected peers and their declared versions.
 */
export declare const PLUGINS_LIST_METHOD = "orca/plugins.list";
/**
 * JSON-RPC error codes specific to the tools surface. Extends the
 * standard -32600..-32099 range.
 */
export declare const toolErrorCodes: {
    /** The named tool is not registered for this plugin. */
    readonly UNKNOWN_TOOL: -32001;
    /** Arguments did not match the declared input_schema. */
    readonly SCHEMA_VIOLATION: -32002;
    /** Handler ran but returned an application error. */
    readonly HANDLER_ERROR: -32003;
};
/**
 * One tool the plugin announces. The fully-qualified id is computed
 * host-side as `<plugin_id>.<name>`.
 */
export interface ToolDeclaration {
    name: string;
    description: string;
    /** JSON Schema document describing the input arguments. */
    input_schema: unknown;
    sensitivity: Sensitivity;
}
/** Wire shape of orca/tools.declare params. */
export interface ToolsDeclareParams {
    tools: ToolDeclaration[];
}
/** Wire shape of orca/tools.declare result — namespaced ids the host accepted. */
export interface ToolsDeclareResult {
    accepted: string[];
}
/**
 * Wire shape of orca/tools.call params. `name` is the bare tool name
 * (no `<plugin_id>.` prefix — the host strips it before dispatch).
 */
export interface ToolCallParams {
    name: string;
    arguments: unknown;
}
/** Wire shape of orca/tools.call result — opaque JSON. */
export interface ToolCallResult {
    result: unknown;
}
/** Wire shape of orca/tools.invoke params. `name` is fq `<peer>.<tool>`. */
export interface ToolInvokeParams {
    name: string;
    arguments: unknown;
    /** Per-call deadline forwarded to the host. */
    timeout_secs?: number;
}
/** Wire shape of orca/tools.invoke result — peer's opaque tool result. */
export interface ToolInvokeResult {
    result: unknown;
}
/** One entry in orca/plugins.list — a connected peer and its version. */
export interface PeerInfo {
    id: string;
    version: string;
}
/** Wire shape of orca/plugins.list result. */
export interface PluginsListResult {
    peers: PeerInfo[];
}
/**
 * Application-level error a tool handler can throw. The transport
 * translates this into a JSON-RPC error response with code
 * `toolErrorCodes.HANDLER_ERROR`. Any other error is treated as internal.
 */
export declare class ToolHandlerError extends Error {
    data?: unknown;
    constructor(message: string, data?: unknown);
}
/**
 * The function shape every tool implementation satisfies. Throw a
 * `ToolHandlerError` to surface an application-level failure with the
 * `HANDLER_ERROR` code.
 */
export type ToolHandler = (args: unknown) => Promise<unknown>;
/**
 * Convenience bundle pairing a declaration with its handler. Stored in
 * the transport's per-connection registry; not part of the wire format.
 */
export interface RegisteredTool {
    declaration: ToolDeclaration;
    handler: ToolHandler;
}
//# sourceMappingURL=tools.d.ts.map