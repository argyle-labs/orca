import { type ErrorObject, type Notification, type Response } from './jsonrpc.js';
import { type NodeBundle } from './pki.js';
export declare const SDK_VERSION = "0.1.0";
export type Flavor = 'full' | 'headless' | 'local';
export type Sensitivity = 'general' | 'sensitive';
export interface HelloParams {
    sdk_version: string;
    plugin_id: string;
    flavor: Flavor;
    core_min_required: string;
    methods_required: string[];
    methods_optional: string[];
}
export interface HelloResult {
    server_version: string;
    ok: boolean;
    status: 'full' | 'degraded' | 'rejected' | string;
    methods: string[];
    reason?: string;
}
export interface TypeDeclaration {
    type_name: string;
    schema_version: string;
    schema: unknown;
    sensitivity: Sensitivity;
}
export interface TypesDeclareResult {
    accepted: string[];
}
export interface TypedValue {
    type: string;
    schema_version: string;
    sensitivity: Sensitivity;
    payload: unknown;
}
export interface ContextEvent {
    subscription_id: string;
    context_id: string;
    value: TypedValue;
}
export declare const CONTEXT_EVENT_METHOD = "orca/context.event";
export interface ConnectOptions {
    /** Host:port (e.g. "127.0.0.1:5051"). */
    addr: string;
    bundle: NodeBundle;
    /** Optional handshake timeout in ms (default 10s). */
    timeoutMs?: number;
}
export declare class Transport {
    private writer;
    private reader;
    private nextID;
    private pending;
    private notifSubs;
    private closed;
    private constructor();
    static connect(opts: ConnectOptions): Promise<Transport>;
    private handleClose;
    private readLoop;
    /** Close the underlying socket. Pending calls reject. */
    close(): void;
    /** Subscribe to every server-pushed notification on this connection. */
    notifications(handler: (n: Notification) => void): () => void;
    /** Send a request and resolve with the matching response. */
    call(method: string, params?: unknown): Promise<Response>;
    hello(pluginID: string, flavor?: Flavor, methodsRequired?: string[], methodsOptional?: string[]): Promise<HelloResult>;
    declareTypes(types: TypeDeclaration[]): Promise<TypesDeclareResult>;
    publishContext(contextID: string, value: TypedValue): Promise<void>;
    subscribeContext(contextID: string, typeFilter?: string[]): Promise<{
        subscriptionID: string;
        events: AsyncIterable<ContextEvent>;
    }>;
    unsubscribeContext(subscriptionID: string): Promise<void>;
}
export type { ErrorObject };
//# sourceMappingURL=transport.d.ts.map