/**
 * JSON-RPC 2.0 wire types. Wire-compatible with projects/sdk/src/jsonrpc.rs
 * and projects/sdk-go/jsonrpc.
 */
export const JSONRPC_VERSION = '2.0';
export const CODE_METHOD_NOT_FOUND = -32601;
export const CODE_INVALID_PARAMS = -32602;
export const CODE_INTERNAL_ERROR = -32603;
export function methodNotFound(method) {
    return { code: CODE_METHOD_NOT_FOUND, message: `method not found: ${method}` };
}
export function invalidParams(detail) {
    return { code: CODE_INVALID_PARAMS, message: `invalid params: ${detail}` };
}
export function internal(detail) {
    return { code: CODE_INTERNAL_ERROR, message: `internal error: ${detail}` };
}
/**
 * Classify a parsed JSON object as request, notification, or response by
 * field shape. Matches the `#[serde(untagged)]` dispatch order on the Rust
 * side: response → notification → request.
 */
export function classifyMessage(raw) {
    if (typeof raw !== 'object' || raw === null) {
        throw new Error('jsonrpc: not an object');
    }
    const obj = raw;
    const hasMethod = typeof obj['method'] === 'string';
    const hasID = obj['id'] !== undefined;
    const hasResultOrErr = obj['result'] !== undefined || obj['error'] !== undefined;
    if (hasID && hasResultOrErr && !hasMethod) {
        return { kind: 'response', value: obj };
    }
    if (hasMethod && !hasID) {
        return { kind: 'notification', value: obj };
    }
    if (hasMethod && hasID) {
        return { kind: 'request', value: obj };
    }
    throw new Error('jsonrpc: unrecognized message shape');
}
//# sourceMappingURL=jsonrpc.js.map