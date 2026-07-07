# Socket.IO surface + network-service plugin abstractions

> Status: **DESIGN / RFC** — review before implementation.
> Driver: the **dockge** plugin revamp. Scope: orca **core** capabilities + how a
> network-service plugin registers and consumes them.

## 1. Principle

orca **core provides capabilities and surfaces** and exposes them to the mesh;
**plugins consume** those capabilities to compose one **unified application
surface**. A plugin holds only its domain logic — never its own transport,
endpoint registry, credential store, or registration boilerplate.

**WebSocket / Socket.IO is a client transport — a way orca *interfaces with a
service*. Nothing else about the model changes.** orca's surfaces (REST · CLI ·
MCP) and the tool/verb surface are unchanged; Socket.IO simply joins HTTP as a
way to reach a service that speaks it.

Separately, *if* orca's own runtime later needs to expose **streaming /
WebSockets** to callers (an inbound surface), that is acceptable — but it is a
distinct concern from this transport and not required by dockge.

## 2. Findings that force this

- **Dockge has no REST API.** It is **Socket.IO v4 / Engine.IO v4** over
  WebSocket; auth is **username/password → JWT returned in the socket ack**,
  authorized **per connection**. (See `dockge-socketio-protocol` memory.)
- The **current dockge plugin client is fictional** — it calls `GET /api/stacks`
  etc. with a bearer token against a wiremock mock. It cannot talk to a real
  dockge and must be rewritten.
- **docker vs dockge:** docker is **machine-local** (drives the local socket;
  tools are `local_only`, nothing over the network). **dockge is a network
  service** (its Socket.IO server) — which is why dockge, not docker, gets the
  endpoint + credentials + remote-client treatment.

## 3. Core additions (plugin-toolkit)

### 3.1 Socket.IO / WebSocket client (`plugin_toolkit::socketio`)

A shared async client, the socket analogue of the existing single HTTP home
(`plugin_toolkit::http`, per `shared-http-streaming-and-buffered`). No plugin
hand-rolls a socket client.

```rust
// sketch — a persistent, authenticated Socket.IO session
pub struct SocketSession { /* wraps rust_socketio async client (EIO v4) */ }

pub struct SocketConfig {
    pub url: String,                 // one resolved address (see 3.2 for fallback)
    pub accept_invalid_certs: bool,  // homelab self-signed wss is common
    pub connect_timeout: Duration,
}

impl SocketSession {
    pub async fn connect(cfg: SocketConfig) -> Result<Self>;
    /// emit an event and await its ack (JSON args array)
    pub async fn emit_ack(&self, event: &str, args: Value, timeout: Duration) -> Result<Value>;
    /// subscribe to a pushed server event (e.g. `stackList`, `terminalWrite`)
    pub async fn on(&self, event: &str, handler: impl Fn(Value) + Send + 'static);
    pub async fn reconnect(&self) -> Result<()>;
}
```

Notes: `rust_socketio` async, EIO v4 (v0.4+); ack-based login; **self-signed TLS
over `wss` is the known snag** — the client must expose an accept-invalid-certs
path. Keep one connected+authenticated session per endpoint for its lifetime
(auth is per-connection); on reconnect re-auth.

**Keep it as agnostic as possible.** The client is a generic Socket.IO/WS
transport, not a dockge client — event names/payloads stay in the plugin. Design
it to serve as many services/surfaces as possible (arbitrary events, ack + push,
namespaces), so any future socket-speaking plugin reuses it unchanged.

### 3.1a In-memory response cache (TTL)

Each orca instance must **answer callers quickly** without a network round-trip
per call. Cache remote reads (e.g. stack lists, statuses) in an **in-memory cache
with a respectable TTL**; serve from cache within the TTL, refresh on miss/expiry.
This is a generic core helper (keyed by endpoint + query), not dockge-specific —
it also fronts HTTP-backed plugins. Writes/actions invalidate the relevant keys.

### 3.2 Endpoint model: multi-address + secrets-backed creds

Generalize the existing `endpoint_resource!` seam so **every** network plugin
gets, for free:

- **Multiple reachable addresses** per endpoint, tried in order with fallback
  (LAN DNS, mesh/VPN, direct `ip:port`) — per `proxmox-endpoint-multi-address-fallback`.
- **Credentials as `SecretRef`, never plaintext in the row** — per
  `plugins-use-abstract-secrets-domain`.

```rust
pub struct Endpoint {
    pub name: String,
    pub addresses: Vec<String>,     // ordered; first reachable wins
    pub credential: SecretRef,      // resolved at call time, never stored plaintext
    pub kind: String,               // "dockge", "proxmox", …
}
```

### 3.3 Authenticated endpoint session (the shared shape)

proxmox (API token), dockge (JWT), jellyfin (token) all repeat: *resolve endpoint
→ pick a reachable address → attach creds from secrets → hold a live auth
session*. Lift that into one core helper, transport-generic (HTTP **or** socket):

```rust
pub trait EndpointTransport {                    // impl by http + socketio
    async fn connect(&self, addr: &str, secret: &str) -> Result<Session>;
}
pub async fn open_endpoint(ep: &Endpoint, t: &dyn EndpointTransport) -> Result<Session>;
// tries ep.addresses in order, ep.credential.resolve() for the secret
```

### 3.4 Backend-registration helper

`docker/src/registration.rs` (BackendDef list + prefix dispatch for
topology/unit/runtime/service) is copy-paste every backend-registering plugin
repeats. Provide a toolkit macro/builder that emits `backends_json()` +
`backend_dispatch()` from a declared set of `(domain, trait-impl)` pairs.

## 4. Secrets integration (targets PR #30 facade)

Uses the incoming `plugin_toolkit::secrets` facade (open in orca PR #30):

```rust
// register a dockge instance's password
let sref = secrets::set(
    &secrets::scoped_name("dockge", instance_name, "password"),
    password, Some("dockge login"))?;         // -> SecretRef stored in the endpoint row
// at call time
let password = endpoint.credential.resolve()?; // SecretRef::resolve()
```

The plugin never knows the secrets backend (1Password/native/etc.).

## 5. Dockge registration map (the revamp)

dockge registers these backends (`BackendDef { domain, name, kind, invoke_prefix }`):

| domain | what dockge provides | trait |
| --- | --- | --- |
| **service** | deploy/configure/status/backup/restore of dockge **itself** | `ServiceBackend` |
| **container_runtime** (kind `dockge`) | compose stacks deploy *through* a dockge instance — **reuse the existing domain + a `kind`, do NOT add a `deploy_target` domain** | runtime/deploy-target adapter |
| **unit** | each dockge instance is a managed unit (five-verb lifecycle) | `UnitProvider` |
| **topology** | stacks/containers + network → fleet nesting | `TopologyCollector` |

Plus the endpoint registry (`dockge.{list,detail,create,update,delete}` over
`endpoint_resource!`, now multi-address + `SecretRef`). The stack operations map
to the real Socket.IO events (`requestStackList`, `getStack`,
`saveStack`/`deployStack`, `start/stop/restart/down/updateStack`, `terminalJoin`
for logs) via the §3.1 client — replacing the fictional REST client.

## 6. What moves to core vs stays in dockge

**Core (plugin-toolkit):** Socket.IO client + WS surface; multi-address endpoint
model; secrets-backed credential resolution; authenticated-session helper;
backend-registration helper.

**dockge plugin:** the dockge-specific event names/payloads, the mapping of stacks
→ deploy-target/unit/topology/service semantics, and its descriptor. Nothing else.

## 7. Sequencing

1. Land secrets facade (**PR #30**) — dockge creds depend on it.
2. Core: `plugin_toolkit::socketio` client (+ accept-invalid-certs) and the WS
   surface; multi-address endpoint + `SecretRef`; session + registration helpers.
3. dockge revamp: real Socket.IO client; register service/deploy_target/unit/
   topology; endpoints multi-address + secrets; integration test against a real
   dockge container (from `compose.yml`), replacing the wiremock tests.
4. Retro-fit proxmox/jellyfin/docker onto the shared endpoint/session/registration
   helpers (dedupe), where it reduces boilerplate without churn.

## 8. Decisions (resolved)

1. **WS is a client transport only** — a way to interface with a service; it does
   not change orca's surface model. (Runtime-side streaming/WS is a separate,
   acceptable-if-needed concern.)
2. **Reuse, don't proliferate domains** — dockge's deploy target is
   `container_runtime` + kind `dockge`; **no new `deploy_target` domain.** Hunt
   for existing seams before adding any; reuse the endpoint/session/registration
   helpers across proxmox/docker/dockge/jellyfin.
3. **In-memory cache with a respectable TTL** (§3.1a) so each orca instance
   returns data to callers fast; generic, endpoint+query keyed, action-invalidated.
4. **Keep the transport maximally agnostic** (§3.1) — a generic Socket.IO/WS
   client usable by as many services/surfaces as possible; plugin owns only its
   event vocabulary.
