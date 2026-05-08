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
export const SDK_VERSION = '0.1.0';
export const CONTEXT_EVENT_METHOD = 'orca/context.event';
export class Transport {
    writer;
    reader;
    nextID = 1;
    pending = new Map();
    notifSubs = new Set();
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
                // Server-to-plugin requests are not part of Phase A.
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
        const params = {
            sdk_version: SDK_VERSION,
            plugin_id: pluginID,
            flavor,
            core_min_required: '0.1.0',
            methods_required: methodsRequired,
            methods_optional: methodsOptional,
        };
        const resp = await this.call('orca/hello', params);
        if (resp.error)
            throw new Error(`orca/hello rejected: ${resp.error.message}`);
        const result = resp.result;
        if (!result.ok) {
            throw new Error(`orca/hello: server returned ok=false (status=${result.status}; ${result.reason ?? 'no reason'})`);
        }
        return result;
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