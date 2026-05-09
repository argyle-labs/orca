/**
 * TCP + mTLS transport: plugin (client) side. Wire-compatible with
 * projects/sdk/src/transport.rs and projects/sdk-go/transport.
 *
 * One Transport wraps a mutually-authenticated TLS stream. A reader loop
 * demuxes incoming frames:
 *   - Responses are routed to the matching call() promise via a per-id table.
 *   - Notifications fan out to subscribers registered via notifications().
 */
import { connect as tlsConnect } from 'node:tls';
import { FrameReader, writeFrame } from './framing.js';
import { classifyMessage, JSONRPC_VERSION, } from './jsonrpc.js';
import { clientTlsOptions } from './pki.js';
import { PLUGINS_LIST_METHOD, TOOLS_CALL_METHOD, TOOLS_DECLARE_METHOD, TOOLS_INVOKE_METHOD, ToolHandlerError, toolErrorCodes, } from './tools.js';
export const SDK_VERSION = '0.1.0';
export const CONTEXT_EVENT_METHOD = 'orca/context.event';
export class Transport {
    writer;
    reader;
    nextID = 1;
    pending = new Map();
    notifSubs = new Set();
    /** Tools the plugin has registered for the host to invoke, by bare name. */
    tools = new Map();
    closed = false;
    constructor(socket) {
        this.writer = socket;
        this.reader = new FrameReader(socket);
        socket.on('close', () => this.handleClose(new Error('transport closed')));
        socket.on('error', err => this.handleClose(err));
        void this.readLoop();
    }
    static connect(opts) {
        const [host, portStr] = opts.addr.split(':');
        if (!host || !portStr) {
            return Promise.reject(new Error(`invalid addr: ${opts.addr}`));
        }
        const port = Number(portStr);
        return new Promise((resolve, reject) => {
            const tlsOpts = clientTlsOptions(opts.bundle);
            const socket = tlsConnect(port, host, tlsOpts);
            const timeout = setTimeout(() => {
                socket.destroy();
                reject(new Error(`tls handshake timeout for ${opts.addr}`));
            }, opts.timeoutMs ?? 10_000);
            socket.once('secureConnect', () => {
                clearTimeout(timeout);
                resolve(new Transport(socket));
            });
            socket.once('error', err => {
                clearTimeout(timeout);
                reject(err);
            });
        });
    }
    handleClose(err) {
        if (this.closed)
            return;
        this.closed = true;
        for (const resolver of this.pending.values()) {
            resolver({
                jsonrpc: JSONRPC_VERSION,
                id: null,
                error: { code: -32603, message: err.message },
            });
        }
        this.pending.clear();
        this.notifSubs.clear();
    }
    async readLoop() {
        try {
            while (true) {
                const frame = await this.reader.read();
                if (frame === null) {
                    this.handleClose(new Error('connection closed'));
                    return;
                }
                let parsed;
                try {
                    parsed = JSON.parse(frame.toString('utf8'));
                }
                catch {
                    continue;
                }
                let msg;
                try {
                    msg = classifyMessage(parsed);
                }
                catch {
                    continue;
                }
                if (msg.kind === 'response') {
                    const id = msg.value.id;
                    if (typeof id === 'number') {
                        const resolver = this.pending.get(id);
                        if (resolver) {
                            this.pending.delete(id);
                            resolver(msg.value);
                        }
                    }
                }
                else if (msg.kind === 'notification') {
                    for (const sub of [...this.notifSubs]) {
                        try {
                            sub(msg.value);
                        }
                        catch {
                            /* swallow subscriber errors */
                        }
                    }
                }
                else if (msg.kind === 'request') {
                    // Server→plugin request. Dispatch off the read loop so a slow
                    // handler doesn't stall incoming frames.
                    void this.dispatchIncoming(msg.value);
                }
            }
        }
        catch (err) {
            this.handleClose(err);
        }
    }
    /** Close the underlying socket. Pending calls reject. */
    close() {
        this.writer.end();
    }
    /**
     * Handle a server→plugin request. Currently only `orca/tools.call` is
     * supported; everything else returns method-not-found.
     */
    async dispatchIncoming(req) {
        const resp = await this.buildResponseFor(req);
        try {
            const body = Buffer.from(JSON.stringify(resp), 'utf8');
            await writeFrame(this.writer, body);
        }
        catch {
            /* swallow — connection likely closed */
        }
    }
    async buildResponseFor(req) {
        if (req.method !== TOOLS_CALL_METHOD) {
            return {
                jsonrpc: JSONRPC_VERSION,
                id: req.id,
                error: { code: -32601, message: `method not found: ${req.method}` },
            };
        }
        const params = req.params;
        if (!params || typeof params.name !== 'string') {
            return {
                jsonrpc: JSONRPC_VERSION,
                id: req.id,
                error: { code: -32602, message: 'invalid params: missing name' },
            };
        }
        const reg = this.tools.get(params.name);
        if (!reg) {
            return {
                jsonrpc: JSONRPC_VERSION,
                id: req.id,
                error: {
                    code: toolErrorCodes.UNKNOWN_TOOL,
                    message: `unknown tool: ${params.name}`,
                },
            };
        }
        try {
            const out = await reg.handler(params.arguments);
            const result = { result: out };
            return { jsonrpc: JSONRPC_VERSION, id: req.id, result };
        }
        catch (err) {
            if (err instanceof ToolHandlerError) {
                return {
                    jsonrpc: JSONRPC_VERSION,
                    id: req.id,
                    error: {
                        code: toolErrorCodes.HANDLER_ERROR,
                        message: err.message,
                        data: err.data,
                    },
                };
            }
            return {
                jsonrpc: JSONRPC_VERSION,
                id: req.id,
                error: { code: -32603, message: err.message ?? String(err) },
            };
        }
    }
    /**
     * Register a tool the host can invoke via orca/tools.call. Bare name
     * (no `<plugin_id>.` prefix — the host applies the namespace).
     * Re-registering the same name replaces the previous handler. Call
     * this for each tool, then call {@link declareTools} once.
     */
    registerTool(name, description, inputSchema, sensitivity, handler) {
        const declaration = {
            name,
            description,
            input_schema: inputSchema,
            sensitivity,
        };
        this.tools.set(name, { declaration, handler });
    }
    /**
     * Send the registered tool set via orca/tools.declare. Returns the
     * namespaced ids the host accepted. Idempotent — calling again
     * replaces the host-side set.
     */
    async declareTools() {
        const tools = Array.from(this.tools.values()).map(t => t.declaration);
        const params = { tools };
        const resp = await this.call(TOOLS_DECLARE_METHOD, params);
        if (resp.error) {
            throw new Error(`${TOOLS_DECLARE_METHOD} rejected: ${resp.error.message}`);
        }
        return resp.result;
    }
    /** Subscribe to every server-pushed notification on this connection. */
    notifications(handler) {
        this.notifSubs.add(handler);
        return () => this.notifSubs.delete(handler);
    }
    /** Send a request and resolve with the matching response. */
    async call(method, params) {
        if (this.closed)
            throw new Error('transport closed');
        const id = this.nextID++;
        const req = { jsonrpc: JSONRPC_VERSION, id, method, params };
        const promise = new Promise(resolve => this.pending.set(id, resolve));
        const body = Buffer.from(JSON.stringify(req), 'utf8');
        try {
            await writeFrame(this.writer, body);
        }
        catch (err) {
            this.pending.delete(id);
            throw err;
        }
        return promise;
    }
    async hello(pluginID, flavor = 'headless', methodsRequired = [], methodsOptional = []) {
        return this.helloFull({
            pluginId: pluginID,
            flavor,
            methodsRequired,
            methodsOptional,
        });
    }
    /**
     * Full hello with peer-plugin dependencies and own version. Use this
     * when porting an `orca-plugin.toml` straight through.
     */
    async helloFull(opts) {
        const params = {
            sdk_version: SDK_VERSION,
            plugin_id: opts.pluginId,
            flavor: opts.flavor,
            core_min_required: opts.coreMinRequired ?? '0.1.0',
        };
        if (opts.pluginVersion !== undefined)
            params.plugin_version = opts.pluginVersion;
        if (opts.methodsRequired !== undefined)
            params.methods_required = opts.methodsRequired;
        if (opts.methodsOptional !== undefined)
            params.methods_optional = opts.methodsOptional;
        if (opts.pluginsRequired !== undefined)
            params.plugins_required = opts.pluginsRequired;
        if (opts.pluginsOptional !== undefined)
            params.plugins_optional = opts.pluginsOptional;
        const resp = await this.call('orca/hello', params);
        if (resp.error)
            throw new Error(`orca/hello rejected: ${resp.error.message}`);
        const result = resp.result;
        if (!result.ok) {
            throw new Error(`orca/hello: server returned ok=false (status=${result.status}; ${result.reason ?? 'no reason'})`);
        }
        return result;
    }
    /**
     * Forward a tool call to a peer plugin via the host. `fqName` is
     * `<peer>.<tool>`. The host resolves the owning plugin, dispatches
     * tools.call, and returns the peer's opaque result.
     *
     * `timeoutMs` is the local deadline; it's also forwarded to the host
     * (rounded to whole seconds) so the host applies its own per-call budget.
     */
    async invokeTool(fqName, args, timeoutMs = 30_000) {
        const params = {
            name: fqName,
            arguments: args,
            timeout_secs: Math.max(1, Math.round(timeoutMs / 1000)),
        };
        const resp = await Promise.race([
            this.call(TOOLS_INVOKE_METHOD, params),
            new Promise((_, reject) => setTimeout(() => reject(new Error(`orca/tools.invoke ${fqName} timed out after ${timeoutMs}ms`)), timeoutMs)),
        ]);
        if (resp.error) {
            throw new Error(`orca/tools.invoke ${fqName} failed: ${resp.error.message}`);
        }
        return resp.result.result;
    }
    /** Ask the host which peer plugins are currently connected. */
    async listPeers() {
        const resp = await this.call(PLUGINS_LIST_METHOD, {});
        if (resp.error)
            throw new Error(`orca/plugins.list: ${resp.error.message}`);
        return resp.result.peers;
    }
    async declareTypes(types) {
        const resp = await this.call('orca/types.declare', { types });
        if (resp.error)
            throw new Error(`orca/types.declare rejected: ${resp.error.message}`);
        return resp.result;
    }
    async publishContext(contextID, value) {
        const resp = await this.call('orca/context.publish', { context_id: contextID, value });
        if (resp.error)
            throw new Error(`orca/context.publish rejected: ${resp.error.message}`);
    }
    async subscribeContext(contextID, typeFilter = []) {
        const resp = await this.call('orca/context.subscribe', {
            context_id: contextID,
            type_filter: typeFilter,
        });
        if (resp.error)
            throw new Error(`orca/context.subscribe rejected: ${resp.error.message}`);
        const subscriptionID = resp.result.subscription_id;
        const queue = [];
        const waiters = [];
        let done = false;
        const unsub = this.notifications(n => {
            if (n.method !== CONTEXT_EVENT_METHOD)
                return;
            const ev = n.params;
            if (ev.subscription_id !== subscriptionID)
                return;
            if (waiters.length > 0) {
                waiters.shift()({ value: ev, done: false });
            }
            else {
                queue.push(ev);
            }
        });
        const events = {
            [Symbol.asyncIterator]() {
                return {
                    next() {
                        if (queue.length > 0) {
                            return Promise.resolve({ value: queue.shift(), done: false });
                        }
                        if (done) {
                            return Promise.resolve({ value: undefined, done: true });
                        }
                        return new Promise(resolve => waiters.push(resolve));
                    },
                    return() {
                        done = true;
                        unsub();
                        for (const w of waiters)
                            w({ value: undefined, done: true });
                        waiters.length = 0;
                        return Promise.resolve({ value: undefined, done: true });
                    },
                };
            },
        };
        return { subscriptionID, events };
    }
    async unsubscribeContext(subscriptionID) {
        const resp = await this.call('orca/context.unsubscribe', { subscription_id: subscriptionID });
        if (resp.error)
            throw new Error(`orca/context.unsubscribe rejected: ${resp.error.message}`);
    }
}
//# sourceMappingURL=transport.js.map