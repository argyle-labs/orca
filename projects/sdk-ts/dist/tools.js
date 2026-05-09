/** Method name plugins use to send their tool catalog. */
export const TOOLS_DECLARE_METHOD = 'orca/tools.declare';
/** Method name the host uses to invoke a registered tool. */
export const TOOLS_CALL_METHOD = 'orca/tools.call';
/**
 * JSON-RPC error codes specific to the tools surface. Extends the
 * standard -32600..-32099 range.
 */
export const toolErrorCodes = {
    /** The named tool is not registered for this plugin. */
    UNKNOWN_TOOL: -32001,
    /** Arguments did not match the declared input_schema. */
    SCHEMA_VIOLATION: -32002,
    /** Handler ran but returned an application error. */
    HANDLER_ERROR: -32003,
};
/**
 * Application-level error a tool handler can throw. The transport
 * translates this into a JSON-RPC error response with code
 * `toolErrorCodes.HANDLER_ERROR`. Any other error is treated as internal.
 */
export class ToolHandlerError extends Error {
    data;
    constructor(message, data) {
        super(message);
        this.name = 'ToolHandlerError';
        this.data = data;
    }
}
//# sourceMappingURL=tools.js.map