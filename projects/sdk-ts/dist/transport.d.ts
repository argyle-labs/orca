import { type ErrorObject, type Notification, type Response } from './jsonrpc.js';
import { type NodeBundle } from './pki.js';
import { type PeerInfo, type ToolHandler, type ToolsDeclareResult } from './tools.js';
export declare const SDK_VERSION = "0.1.0";
export type Flavor = 'full' | 'headless' | 'local';
export type Sensitivity = 'general' | 'sensitive';
export interface HelloParams {
    sdk_version: string;
    plugin_id: string;
    /** Plugin's own version, from manifest.plugin.version. */
    plugin_version?: string;
    flavor: Flavor;
    core_min_required: string;
    methods_required?: string[];
    methods_optional?: string[];
    /** Required peer plugins, formatted as "<id>>=<min_version>". */
    plugins_required?: string[];
    /** Optional peer plugins. Same format. */
    plugins_optional?: string[];
}
/**
 * Builder shape for {@link Transport.helloFull}. Mirrors the Rust SDK's
 * HelloOptions — the wire shape is HelloParams; this is the ergonomic
 * input that adds new fields without breaking existing call sites.
 */
export interface HelloOptions {
    pluginId: string;
    pluginVersion?: string;
    flavor: Flavor;
    coreMinRequired?: string;
    methodsRequired?: string[];
    methodsOptional?: string[];
    pluginsRequired?: string[];
    pluginsOptional?: string[];
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
    /** Tools the plugin has registered for the host to invoke, by bare name. */
    private tools;
    private closed;
    private constructor();
    static connect(opts: ConnectOptions): Promise<Transport>;
    private handleClose;
    private readLoop;
    /** Close the underlying socket. Pending calls reject. */
    close(): void;
    /**
     * Handle a server→plugin request. Currently only `orca/tools.call` is
     * supported; everything else returns method-not-found.
     */
    private dispatchIncoming;
    private buildResponseFor;
    /**
     * Register a tool the host can invoke via orca/tools.call. Bare name
     * (no `<plugin_id>.` prefix — the host applies the namespace).
     * Re-registering the same name replaces the previous handler. Call
     * this for each tool, then call {@link declareTools} once.
     */
    registerTool(name: string, description: string, inputSchema: unknown, sensitivity: Sensitivity, handler: ToolHandler): void;
    /**
     * Send the registered tool set via orca/tools.declare. Returns the
     * namespaced ids the host accepted. Idempotent — calling again
     * replaces the host-side set.
     */
    declareTools(): Promise<ToolsDeclareResult>;
    /** Subscribe to every server-pushed notification on this connection. */
    notifications(handler: (n: Notification) => void): () => void;
    /** Send a request and resolve with the matching response. */
    call(method: string, params?: unknown): Promise<Response>;
    hello(pluginID: string, flavor?: Flavor, methodsRequired?: string[], methodsOptional?: string[]): Promise<HelloResult>;
    /**
     * Full hello with peer-plugin dependencies and own version. Use this
     * when porting an `orca-plugin.toml` straight through.
     */
    helloFull(opts: HelloOptions): Promise<HelloResult>;
    /**
     * Forward a tool call to a peer plugin via the host. `fqName` is
     * `<peer>.<tool>`. The host resolves the owning plugin, dispatches
     * tools.call, and returns the peer's opaque result.
     *
     * `timeoutMs` is the local deadline; it's also forwarded to the host
     * (rounded to whole seconds) so the host applies its own per-call budget.
     */
    invokeTool(fqName: string, args: unknown, timeoutMs?: number): Promise<unknown>;
    /** Ask the host which peer plugins are currently connected. */
    listPeers(): Promise<PeerInfo[]>;
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