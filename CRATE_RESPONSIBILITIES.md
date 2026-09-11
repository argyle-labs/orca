# Crate Responsibilities

The single source of truth for what each crate owns. **Before adding code, find
the crate that already owns the domain.** Before adding a new crate, justify
here why none of the existing crates can absorb the responsibility.

The authoritative member list is `Cargo.toml [workspace.members]`. Every crate
lives under `projects/` with a flat package name (no `orca-` prefix).

> **Status: TARGET model.** This document describes the domain-oriented target
> the workspace is migrating toward (23 crates, down from ~36). The migration is
> staged bottom-up; the DB data-access seam (PR #436) is the first step and is
> merged. Where a crate below has not been carved/renamed yet, this doc is the
> north star for where its code belongs. See the roadmap at the end.

## Principles

1. **Domain-driven, not too many crates.** Each crate is a coherent capability with a clear delineation of responsibility — not a per-feature dumping ground.
2. **Each domain owns its storage.** A domain owns its own tables and CRUD. `database` holds only primitives (pool, schema bootstrap, migrations, replication). There are **no `*-store` helper crates** — storage is not a separate layer, it's part of the domain that owns the concept.
3. **One owner per responsibility.** If two crates implement the same primitive (hashing, http client, path resolution), one is wrong.
4. **Dependencies point down** along a single acyclic spine (below). No upward edges, no sibling cycles.
5. **Seams are the norm.** Nearly every domain exposes a plugin provider/consumer seam so a plugin can register providers or contribute data. Only the foundation, `authentication`, `systems`, `plugins`, and `server` are internal.
6. **Names plural where natural.** `hosts`, `systems`, `configs`, `deployments`, `models`, `identities` — the crate manages many of the thing.

---

## Dependency spine (low → high)

```
database, utils
  → contracts
    → sdk
      → identities
        → { authentication, authorization }
          → hosts
            → systems
              → capabilities (deployments · models · agents · files ·
                               specs · notifications · mcp · media ·
                               secrets · storage · configs)
                → server
                  → topology
```

A crate **MAY** depend on anything strictly below it on the spine. It **MUST
NOT** depend on anything at or above. `topology` sits at the very top: it is the
one crate allowed to depend on every domain, because nothing depends on it.

---

## Foundation (4) — pure, everything depends on them

### `utils`
Pure, stateless cross-cutting utilities. **No tools, no tables.** Hashing, http
helpers, time, path helpers (`expand_tilde`), mesh-probe, framing, JSON-schema,
jsonrpc, PKI, search, shutdown, state, git, the periodic scheduler, pure version
math, generic tool **dispatch/routing** (⟵ `dispatch`), the pure filesystem
helpers (⟵ `files`), and the stateless spec **parsers/clients**: OpenAPI
parse/navigate (⟵ `openapi`) and the GraphQL client/query-builder over the shared
http transport (⟵ `graphql`). Every other crate may import this.

### `database`  *(rename ⟵ `db`)*
Storage machinery **only**: the connection pool / data-access seam
(`Db` read/write/tx + async wrappers over a reader pool and single writer),
schema bootstrap, migrations, and the replication engine. **Owns no domain
tables.** Domains build their CRUD on this; they do not open their own
connections.

> **Never WAL.** SQLCipher runs `journal_mode=DELETE`, `synchronous=FULL`,
> `busy_timeout=5000`, and `PRAGMA query_only=ON` on readers. WAL + multiple
> connections yields short reads on `-shm` (error 522). The reader pool +
> single writer live behind the `Db` seam; all DB work goes through it via
> `spawn_blocking`.

### `contracts`  *(rename ⟵ `contract`)*
Cold shared types + service-trait **seams**, no tokio/axum/inventory. `ToolCtx`,
`OrcaTool`/`OrcaToolDef`, `CallerIdentity`, `OrcaError`, the generic `Status`
type, and the generic **namespacing seam** any crate uses to group its resources
(`home`/`work`/`personal`). The stable leaf the macros and dispatch anchor to.

### `sdk`
The **universal tool-authoring + dev** crate. Every tool-defining crate (core or
plugin) depends on it. Absorbs: the `#[orca_tool]` proc-macros + registration
runtime (⟵ `derive` + `macro-runtime`), the toolkit facade, plugin wire types
(⟵ `plugin-abi` + `plugin-proto`), codegen (⟵ `plugin-toolkit-build`), native
FFI/UniFFI bindings (⟵ `app-kit`), and the **dev-overlay** (⟵ `dev`, see
Cross-cutting).

---

## Identity & access — the IAM triad + secrets

### `identities`  🔌 seam
**Who exists.** Every principal — users, **groups**, hosts, systems/peers,
plugins — plus membership and the PKI cert-identity binding. Lowest domain.
**Seam:** orca as a central identity provider (SSO); plugins can **expose**
external identity sources (LDAP/OIDC, a service's own user/group model) into
orca **and** have them controllable *through* the plugin (create/update/disable
in the external source).

### `authentication`  *(⟵ `auth`, authn half)*
**Proving** an identity: credentials/login, sessions, tokens, OAuth, PKI/mTLS
handshake. Internal — proving is orca's own trust boundary; the pluggable part
(external directories, SSO tie-ins) lives in `identities`/`authorization`.

### `authorization`  🔌 seam  *(⟵ `auth`, authz half + `dispatch` role table)*
**What an identity may do:** roles/RBAC (the role table), permissions, access
grants (user/group → service/resource). **Seam:** service access + SSO tie-ins.

### `secrets`  🔌 seam
Identity-scoped, **multi-backend** secret storage. Personal creds
(Sonarr/Radarr/…) belong to a user's identity; also system/service secrets.
**Seam:** pluggable backend per secret/identity — 1Password, native keychain,
Bitwarden/Vaultwarden, config-DB fallback (all live). Consumers
(mcp/deployments/plugins/authentication) **fetch by (identity, service)** and
never store creds themselves.

---

## Machine & infrastructure

### `hosts`  🔌 seam
The reachable / capable / healthy view of **any addressable endpoint** — not
just a machine: a machine, a service (docker/vm/lxc), or an exposed API
(OPNsense/AdGuard/Caddy/Proxmox). Addressing, capabilities, status, keyed by an
identity. The status *type* is generic (`contracts`); `hosts` owns the status
*data*. **This is the node.** **Seam:** a plugin contributes
addressing/capability/status for the endpoints it fronts.

### `systems`  *(absorbs `pod`; hardware ⟵ `system_info`)*
A managed orca node: the hardware snapshot, local lifecycle (install / update /
daemon / remediation / diagnostics), **and the mesh** — peers are just other
systems: discovery, pairing, trust, mTLS, cert-rotation, exec, and roster +
status replication (the dissolved `pod`). Internal. **`systems` must stand
alone** — peer-consuming logic that used to reach into a pod-store is elevated
here rather than depended on downward. Home of the deferred anti-entropy /
gossip backstop idea (see Messaging).

### `configs`  🔌 seam  *(⟵ `config_store` + `config-source`)*
Declarative configuration: a per-noun **schema registry** + **reconcile**
(status / diff / apply / pull / push / sync) over **multiple config sources the
user chooses** — DB-backed config rows, on-system config files, and git. *Git is
one path, not the store.* **Seam:** plugins register their own config
nouns/schema and can supply their own config source.

### `storage`  🔌 seam
Storage adapter trait + registry. **Seam:** storage backends — NFS / SMB / S3
(already running locally) / … Keep storage logic orca-side; plugin backends stay
thin (fstype grammar + mount commands).

---

## Capabilities

### `deployments`  🔌 seam  *(⟵ `service` + `deploy-target` + `containers`)*
**What runs and where.** The deployable-service model + adapter, the
deploy-target registry, and **both substrates**: containers (Docker/LXC/Podman)
and VMs (Proxmox VMs/LXC, including VM↔container transfer). **Seam:** substrate /
deploy-target adapters (docker, proxmox, … register as providers). A deployment
*produces* a `hosts` entry; `deployments` owns lifecycle, `hosts` owns "is it
reachable/healthy."

### `models`  🔌 seam  *(rename ⟵ `model`)*
LLM engine + backend abstraction (Claude / LMStudio / Ollama) + registry.
**Seam:** engine/backend providers.

### `agents`  🔌 seam  *(⟵ `agents` + `conversation`)*
Agent tool registry + composition, interactive session / REPL state, and
background agent-job management. **Seam:** agent tool providers.

### `files`  🔌 seam
Thin fs tool surface: `fs.{list,read,tree,search,stat}` + the embedded vault.
(Its pure helpers moved down to `utils`.) **Seam:** file / vault providers.

### `specs`  🔌 seam  *(⟵ `spec`, slimmed)*
The spec **registry**: OpenAPI/GraphQL specs as first-class managed objects +
`spec.*` tools. Uses the parsers/clients that now live in `utils`. **Seam:**
spec providers.

### `notifications`  🔌 seam
Backend-agnostic event dispatch → backends. **Seam:** plugins emit their own
notifications/events through it **and** register delivery backends.

### `mcp`  🔌 seam
orca as an MCP **client**: the `McpPool` connecting to and calling **external**
MCP servers; fetches needed credentials from `secrets`. **Seam:**
plugin-registered MCP servers. Distinct from orca *serving* its own
`#[orca_tool]`s over MCP (that is `sdk`/dispatch/`server`).

### `media`  🔌 seam
Generic media capability domain: media **types** (audiobook / ebook / comic /
movie / music / …) × **roles** {`downloaded_by`, `served_by`}, **plus
`located_at`** — the physical file location (path / mount / host), resolved
through `storage`. **Location is 1..N copies:** one location is the common, valid
case; with storage replication (Syncthing, NFS/SMB/PBS/S3) the same item
reasonably lives in ≥2 places; a *missing* replica is an anomaly **only where
replication is expected** for that item. Tools: `media.{list, downloaded-by,
served-by, located-at}`. **Seams:** downloaders/indexers register as
`downloaded-by`; servers/libraries as `served-by`. Not a single "media plugin" —
the domain is generic, plugins fill roles.

---

## Plugin host (1)

### `plugins`  *(⟵ `plugin-loader` + `runtime`)*
Spawn / supervise subprocess plugins: handshake, register their
tools/backends/schema, route invokes, serve host capabilities, plus the plugin
registry / install / KV store. The authoring side lives in the universal `sdk`.
Internal — this is the *host* that runs seams, not a seam.

---

## Surface (2)

### `server`
The `orca` binary: HTTP / MCP / CLI entry, `ToolCtx` wiring, the daemon
supervisor, and the dev proxy. **No business logic.**

### `topology`  🔌 seam  *(⟵ `orca-inventory` + `inventory-tests`)*
The composed cross-domain view: the **graph** over hosts / systems / deployments
/ storage / media — nodes come from `hosts`, edges from the higher domains —
plus the cross-crate integration tests that link every domain. **Seam:** a plugin
exposes info to inform its place in the topology. At the top of the spine so it
may depend on everything.

---

## Cross-cutting

### Plugin seams — the norm, not the exception
14 seam domains: `identities`, `authorization`, `secrets`, `hosts`, `storage`,
`configs`, `topology`, `deployments`, `models`, `agents`, `files`, `specs`,
`notifications`, `mcp`, `media`. Internal (no seam): the foundation
(`utils`/`database`/`contracts`/`sdk`), `authentication`, `systems`, `plugins`,
`server`.

### Dev-overlay (in `sdk`)  *(⟵ `dev`)*
`orca dev` runs a dev build that **supersedes the installed process only while
running** and reverts cleanly on stop/crash. **Additive & reversible** — never
uninstalls or kills the installed daemon; the installed process resumes if dev
exits, so the host is never left bare. Covers the core daemon **and** any plugin
(`plugins` prefers a dev build while active). Runs **bare-metal or in a Docker
container (preferred)**. Drives `deployments` (container), `server` (dev proxy),
`systems` (daemon lifecycle), `plugins` (dev-build preference).

### Messaging / broker — decision: no broker domain
Reconcile stays the **level-triggered source of truth** (self-healing; converges
after missed events/partitions). A broker is edge-triggered and does not replace
reconcile; a *central* broker is a rejected single point of failure (it would
regress the "orca must never bring down the host it manages" rule). Brokerless
**gossip / anti-entropy** is the right pattern *if* mesh replication ever strains
at scale, and is deferred into `systems`; the cheap 90% is ensuring mesh status
replication has a periodic full-state anti-entropy backstop. A `messaging` seam
remains a **candidate**, scoped to *external ingest only* (MQTT / Home Assistant
→ orca reacts). Running a broker as a service is already covered by
`deployments` + `hosts` + `configs` + `secrets`.

---

## Totals

**4 foundation + 16 domains + 1 plugin-host + 2 surface = 23 crates** (from ~36).

Domains (16): identities, authentication, authorization, secrets, hosts,
systems, configs, storage, deployments, models, agents, files, specs,
notifications, mcp, media.

### Dissolutions / merges / renames
- **Dissolved:** `pod` → `systems` (a peer is another system); `namespace` →
  generic seam in `contracts` (sharing → `authorization`, membership per-crate);
  `dispatch` routing → `utils`, its role table → `identities`; `macros`
  (`derive` + `macro-runtime`) → `sdk`; `app-kit` → `sdk`; `dev` → `sdk`.
- **Merges:** `service` + `deploy-target` + `containers` → `deployments`;
  `agents` + `conversation` → `agents`; `config_store` + `config-source` →
  `configs`; `plugin-abi` + `plugin-proto` + `plugin-toolkit` +
  `plugin-toolkit-build` → `sdk`; `plugin-loader` + `runtime` → `plugins`;
  `orca-inventory` + `inventory-tests` → `topology`. **Spec split:** OpenAPI/
  GraphQL parsers/clients → `utils`; the `spec` registry + tools → `specs`.
- **Renames:** `db` → `database`; `contract` → `contracts`; `inventory` →
  `topology`; `model` → `models`.
- **Data moves:** `SystemInfoReport` mesh fields
  (`pod_peer_count`/`pod_paired_count`/`self_secure`) are write-only → delete;
  the `system_info` hardware snapshot → `systems`.

---

## Migration roadmap (bottom-up; each PR compiles green)

1. **Db data-access seam + reader-pool concurrency fix** — ✅ merged, PR #436.
2. **Foundation renames/folds** — `db`→`database`, `contract`→`contracts`;
   `dispatch` + files-helpers + spec-parsers → `utils`; `macros`/`app-kit`/`dev`
   → `sdk`.
3. **IAM triad** — split `auth` → `identities` / `authentication` /
   `authorization`; re-home `claim_identity` to `identities`.
4. **hosts + systems** — carve `hosts`; fold `pod` into `systems`; move the
   `system_info` hardware snapshot in; delete the write-only mesh-count fields.
5. **configs** — merge `config_store` + `config-source`; route call sites onto
   the seam.
6. **Capability merges** — `deployments`, `agents`, `media` (add `located_at`),
   `specs` slim, `model`→`models`.
7. **plugins & surface** — `plugin-loader` + `runtime` → `plugins`;
   `orca-inventory` + `inventory-tests` → `topology`.
8. **Dev-overlay** — the bare-metal / container dev server in `sdk`.
