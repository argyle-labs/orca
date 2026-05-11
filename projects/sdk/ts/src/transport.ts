/**
 * TCP + mTLS transport: plugin (client) side. Wire-compatible with
 * projects/sdk/src/transport.rs and projects/sdk-go/transport.
 *
 * One Transport wraps a mutually-authenticated TLS stream. A reader loop
 * demuxes incoming frames:
 *   - Responses are routed to the matching call() promise via a per-id table.
 *   - Notifications fan out to subscribers registered via notifications().
 */
import { connect as tlsConnect, type TLSSocket } from 'node:tls';
import { FrameReader, writeFrame } from './framing.js';
import {
  classifyMessage,
  type ErrorObject,
  JSONRPC_VERSION,
  type Notification,
  type Request,
  type Response,
} from './jsonrpc.js';
import { clientTlsOptions, type NodeBundle } from './pki.js';
import {
  type RegisteredTool,
  type PeerInfo,
  type PluginsListResult,
  type ToolInvokeParams,
  type ToolInvokeResult,
  PLUGINS_LIST_METHOD,
  TOOLS_CALL_METHOD,
  TOOLS_DECLARE_METHOD,
  TOOLS_INVOKE_METHOD,
  type ToolCallParams,
  type ToolCallResult,
  type ToolDeclaration,
  ToolHandlerError,
  type ToolHandler,
  type ToolsDeclareParams,
  type ToolsDeclareResult,
  toolErrorCodes,
} from './tools.js';

export const SDK_VERSION = '0.1.0';

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

export const CONTEXT_EVENT_METHOD = 'orca/context.event';

export interface ConnectOptions {
  /** Host:port (e.g. "127.0.0.1:5051"). */
  addr: string;
  bundle: NodeBundle;
  /** Optional handshake timeout in ms (default 10s). */
  timeoutMs?: number;
}

export class Transport {
  private writer: TLSSocket;
  private reader: FrameReader;
  private nextID = 1;
  private pending = new Map<number, (resp: Response) => void>();
  private notifSubs = new Set<(n: Notification) => void>();
  /** Tools the plugin has registered for the host to invoke, by bare name. */
  private tools = new Map<string, RegisteredTool>();
  private closed = false;

  private constructor(socket: TLSSocket) {
    this.writer = socket;
    this.reader = new FrameReader(socket);
    socket.on('close', () => this.handleClose(new Error('transport closed')));
    socket.on('error', err => this.handleClose(err));
    void this.readLoop();
  }

  static connect(opts: ConnectOptions): Promise<Transport> {
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

  private handleClose(err: Error): void {
    if (this.closed) return;
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

  private async readLoop(): Promise<void> {
    try {
      while (true) {
        const frame = await this.reader.read();
        if (frame === null) {
          this.handleClose(new Error('connection closed'));
          return;
        }
        let parsed: unknown;
        try {
          parsed = JSON.parse(frame.toString('utf8'));
        } catch {
          continue;
        }
        let msg;
        try {
          msg = classifyMessage(parsed);
        } catch {
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
        } else if (msg.kind === 'notification') {
          for (const sub of [...this.notifSubs]) {
            try {
              sub(msg.value);
            } catch {
              /* swallow subscriber errors */
            }
          }
        } else if (msg.kind === 'request') {
          // Server→plugin request. Dispatch off the read loop so a slow
          // handler doesn't stall incoming frames.
          void this.dispatchIncoming(msg.value);
        }
      }
    } catch (err) {
      this.handleClose(err as Error);
    }
  }

  /** Close the underlying socket. Pending calls reject. */
  close(): void {
    this.writer.end();
  }

  /**
   * Handle a server→plugin request. Currently only `orca/tools.call` is
   * supported; everything else returns method-not-found.
   */
  private async dispatchIncoming(req: Request): Promise<void> {
    const resp = await this.buildResponseFor(req);
    try {
      const body = Buffer.from(JSON.stringify(resp), 'utf8');
      await writeFrame(this.writer, body);
    } catch {
      /* swallow — connection likely closed */
    }
  }

  private async buildResponseFor(req: Request): Promise<Response> {
    if (req.method !== TOOLS_CALL_METHOD) {
      return {
        jsonrpc: JSONRPC_VERSION,
        id: req.id,
        error: { code: -32601, message: `method not found: ${req.method}` },
      };
    }
    const params = req.params as ToolCallParams | undefined;
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
      const result: ToolCallResult = { result: out };
      return { jsonrpc: JSONRPC_VERSION, id: req.id, result };
    } catch (err) {
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
        error: { code: -32603, message: (err as Error).message ?? String(err) },
      };
    }
  }

  /**
   * Register a tool the host can invoke via orca/tools.call. Bare name
   * (no `<plugin_id>.` prefix — the host applies the namespace).
   * Re-registering the same name replaces the previous handler. Call
   * this for each tool, then call {@link declareTools} once.
   */
  registerTool(
    name: string,
    description: string,
    inputSchema: unknown,
    sensitivity: Sensitivity,
    handler: ToolHandler,
  ): void {
    const declaration: ToolDeclaration = {
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
  async declareTools(): Promise<ToolsDeclareResult> {
    const tools = Array.from(this.tools.values()).map(t => t.declaration);
    const params: ToolsDeclareParams = { tools };
    const resp = await this.call(TOOLS_DECLARE_METHOD, params);
    if (resp.error) {
      throw new Error(`${TOOLS_DECLARE_METHOD} rejected: ${resp.error.message}`);
    }
    return resp.result as ToolsDeclareResult;
  }

  /** Subscribe to every server-pushed notification on this connection. */
  notifications(handler: (n: Notification) => void): () => void {
    this.notifSubs.add(handler);
    return () => this.notifSubs.delete(handler);
  }

  /** Send a request and resolve with the matching response. */
  async call(method: string, params?: unknown): Promise<Response> {
    if (this.closed) throw new Error('transport closed');
    const id = this.nextID++;
    const req: Request = { jsonrpc: JSONRPC_VERSION, id, method, params };
    const promise = new Promise<Response>(resolve => this.pending.set(id, resolve));
    const body = Buffer.from(JSON.stringify(req), 'utf8');
    try {
      await writeFrame(this.writer, body);
    } catch (err) {
      this.pending.delete(id);
      throw err;
    }
    return promise;
  }

  async hello(
    pluginID: string,
    flavor: Flavor = 'headless',
    methodsRequired: string[] = [],
    methodsOptional: string[] = [],
  ): Promise<HelloResult> {
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
  async helloFull(opts: HelloOptions): Promise<HelloResult> {
    const params: HelloParams = {
      sdk_version: SDK_VERSION,
      plugin_id: opts.pluginId,
      flavor: opts.flavor,
      core_min_required: opts.coreMinRequired ?? '0.1.0',
    };
    if (opts.pluginVersion !== undefined) params.plugin_version = opts.pluginVersion;
    if (opts.methodsRequired !== undefined) params.methods_required = opts.methodsRequired;
    if (opts.methodsOptional !== undefined) params.methods_optional = opts.methodsOptional;
    if (opts.pluginsRequired !== undefined) params.plugins_required = opts.pluginsRequired;
    if (opts.pluginsOptional !== undefined) params.plugins_optional = opts.pluginsOptional;
    const resp = await this.call('orca/hello', params);
    if (resp.error) throw new Error(`orca/hello rejected: ${resp.error.message}`);
    const result = resp.result as HelloResult;
    if (!result.ok) {
      throw new Error(
        `orca/hello: server returned ok=false (status=${result.status}; ${result.reason ?? 'no reason'})`,
      );
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
  async invokeTool(fqName: string, args: unknown, timeoutMs = 30_000): Promise<unknown> {
    const params: ToolInvokeParams = {
      name: fqName,
      arguments: args,
      timeout_secs: Math.max(1, Math.round(timeoutMs / 1000)),
    };
    const resp = await Promise.race<Response>([
      this.call(TOOLS_INVOKE_METHOD, params),
      new Promise<never>((_, reject) =>
        setTimeout(
          () => reject(new Error(`orca/tools.invoke ${fqName} timed out after ${timeoutMs}ms`)),
          timeoutMs,
        ),
      ),
    ]);
    if (resp.error) {
      throw new Error(`orca/tools.invoke ${fqName} failed: ${resp.error.message}`);
    }
    return (resp.result as ToolInvokeResult).result;
  }

  /** Ask the host which peer plugins are currently connected. */
  async listPeers(): Promise<PeerInfo[]> {
    const resp = await this.call(PLUGINS_LIST_METHOD, {});
    if (resp.error) throw new Error(`orca/plugins.list: ${resp.error.message}`);
    return (resp.result as PluginsListResult).peers;
  }

  async declareTypes(types: TypeDeclaration[]): Promise<TypesDeclareResult> {
    const resp = await this.call('orca/types.declare', { types });
    if (resp.error) throw new Error(`orca/types.declare rejected: ${resp.error.message}`);
    return resp.result as TypesDeclareResult;
  }

  async publishContext(contextID: string, value: TypedValue): Promise<void> {
    const resp = await this.call('orca/context.publish', { context_id: contextID, value });
    if (resp.error) throw new Error(`orca/context.publish rejected: ${resp.error.message}`);
  }

  async subscribeContext(
    contextID: string,
    typeFilter: string[] = [],
  ): Promise<{ subscriptionID: string; events: AsyncIterable<ContextEvent> }> {
    const resp = await this.call('orca/context.subscribe', {
      context_id: contextID,
      type_filter: typeFilter,
    });
    if (resp.error) throw new Error(`orca/context.subscribe rejected: ${resp.error.message}`);
    const subscriptionID = (resp.result as { subscription_id: string }).subscription_id;

    const queue: ContextEvent[] = [];
    const waiters: Array<(ev: IteratorResult<ContextEvent>) => void> = [];
    let done = false;

    const unsub = this.notifications(n => {
      if (n.method !== CONTEXT_EVENT_METHOD) return;
      const ev = n.params as ContextEvent;
      if (ev.subscription_id !== subscriptionID) return;
      if (waiters.length > 0) {
        waiters.shift()!({ value: ev, done: false });
      } else {
        queue.push(ev);
      }
    });

    const events: AsyncIterable<ContextEvent> = {
      [Symbol.asyncIterator](): AsyncIterator<ContextEvent> {
        return {
          next() {
            if (queue.length > 0) {
              return Promise.resolve({ value: queue.shift()!, done: false });
            }
            if (done) {
              return Promise.resolve({ value: undefined, done: true });
            }
            return new Promise(resolve => waiters.push(resolve));
          },
          return() {
            done = true;
            unsub();
            for (const w of waiters) w({ value: undefined, done: true });
            waiters.length = 0;
            return Promise.resolve({ value: undefined, done: true });
          },
        };
      },
    };
    return { subscriptionID, events };
  }

  async unsubscribeContext(subscriptionID: string): Promise<void> {
    const resp = await this.call('orca/context.unsubscribe', { subscription_id: subscriptionID });
    if (resp.error) throw new Error(`orca/context.unsubscribe rejected: ${resp.error.message}`);
  }
}

export type { ErrorObject };
