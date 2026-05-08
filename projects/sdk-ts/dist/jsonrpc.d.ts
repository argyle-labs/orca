/**
 * JSON-RPC 2.0 wire types. Wire-compatible with projects/sdk/src/jsonrpc.rs
 * and projects/sdk-go/jsonrpc.
 */
export declare const JSONRPC_VERSION = "2.0";
export interface Request {
    jsonrpc: typeof JSONRPC_VERSION;
    id: number | string;
    method: string;
    params?: unknown;
}
export interface Notification {
    jsonrpc: typeof JSONRPC_VERSION;
    method: string;
    params?: unknown;
}
export interface ErrorObject {
    code: number;
    message: string;
    data?: unknown;
}
export interface Response {
    jsonrpc: typeof JSONRPC_VERSION;
    id: number | string | null;
    result?: unknown;
    error?: ErrorObject;
}
export declare const CODE_METHOD_NOT_FOUND = -32601;
export declare const CODE_INVALID_PARAMS = -32602;
export declare const CODE_INTERNAL_ERROR = -32603;
export declare function methodNotFound(method: string): ErrorObject;
export declare function invalidParams(detail: string): ErrorObject;
export declare function internal(detail: string): ErrorObject;
export type Message = {
    kind: 'request';
    value: Request;
} | {
    kind: 'notification';
    value: Notification;
} | {
    kind: 'response';
    value: Response;
};
/**
 * Classify a parsed JSON object as request, notification, or response by
 * field shape. Matches the `#[serde(untagged)]` dispatch order on the Rust
 * side: response → notification → request.
 */
export declare function classifyMessage(raw: unknown): Message;
//# sourceMappingURL=jsonrpc.d.ts.map