# Namespace Consolidation Plan

Current state: 34 distinct orca-native prefixes. Target: 8 top-level domains.

---

## Mapping — Current → Target

### `system.*` (absorbs: system, pod, config, auth, host, host_status, pki, db, schedule, secret, agent-backend, engine, plugin, plugin-data, mcp, mcp-federation, sweep, infra)

| Current | Target |
|---------|--------|
| `system.detail` | `system.detail` ✓ |
| `system.health` | `system.health` ✓ |
| `system.lifecycle.update` | `system.lifecycle.update` ✓ |
| `system.dev.update` | `system.dev.update` ✓ |
| `system.diagnostic.list` | `system.diagnostic.list` ✓ |
| `system.runtime.detail` | `system.runtime.detail` ✓ |
| `system.update.*` | `system.update.*` ✓ |
| `pod.detail` | `system.pod.detail` |
| `pod.dev.update` | `system.dev.update` (merge) |
| `pod.discovery.list` | `system.peer.discovery.list` |
| `pod.handshake.create` | `system.peer.handshake.create` |
| `pod.handshake.list` | `system.peer.handshake.list` |
| `pod.invite.create` | `system.peer.invite.create` |
| `pod.join.create` | `system.peer.join.create` |
| `pod.peer.delete` | `system.peer.delete` |
| `pod.peer.detail` | `system.peer.detail` |
| `pod.peer.list` | `system.peer.list` |
| `pod.peer.update` | `system.peer.update` |
| `config.delete` | `system.config.delete` |
| `config.get` | `system.config.get` |
| `config.list` | `system.config.list` |
| `config.set` | `system.config.set` |
| `auth.session.create` | `system.auth.session.create` |
| `auth.session.delete` | `system.auth.session.delete` |
| `auth.session.detail` | `system.auth.session.detail` |
| `auth.token.create` | `system.auth.token.create` |
| `auth.token.delete` | `system.auth.token.delete` |
| `auth.token.list` | `system.auth.token.list` |
| `host.detail` | `system.host.detail` |
| `host.refresh` | `system.host.refresh` |
| `host.set` | `system.host.set` |
| `host_status.detail` | `system.host.status.detail` |
| `host_status.list` | `system.host.status.list` |
| `pki.ca.create` | `system.pki.ca.create` |
| `pki.cert.create` | `system.pki.cert.create` |
| `pki.list` | `system.pki.list` |
| `db.detail` | `system.db.detail` |
| `db.lifecycle.update` | `system.db.lifecycle.update` |
| `schedule.list` | `system.schedule.list` |
| `schedule.run` | `system.schedule.run` |
| `schedule.status` | `system.schedule.status` |
| `secret.backends` | `system.secret.backends` |
| `secret.delete` | `system.secret.delete` |
| `secret.detail` | `system.secret.detail` |
| `secret.list` | `system.secret.list` |
| `secret.set` | `system.secret.set` |
| `agent-backend.clear-key` | `system.agent.backend.clear-key` |
| `agent-backend.detail` | `system.agent.backend.detail` |
| `agent-backend.override` | `system.agent.backend.override` |
| `agent-backend.set-key` | `system.agent.backend.set-key` |
| `agent-backend.set-mode` | `system.agent.backend.set-mode` |
| `agent-backend.use-server-anthropic` | `system.agent.backend.use-server-anthropic` |
| `agents.get` | `system.agent.get` |
| `agents.get-config` | `system.agent.get-config` |
| `agents.get-context` | `system.agent.get-context` |
| `agents.list` | `system.agent.list` |
| `agents.search-logs` | `system.agent.search-logs` |
| `engine.create` | `system.engine.create` |
| `engine.delete` | `system.engine.delete` |
| `engine.disable` | `system.engine.update` (enable=false) |
| `engine.enable` | `system.engine.update` (enable=true) |
| `engine.list` | `system.engine.list` |
| `plugin.create` | `system.plugin.create` |
| `plugin.delete` | `system.plugin.delete` |
| `plugin.disable` | `system.plugin.update` (enable=false) |
| `plugin.enable` | `system.plugin.update` (enable=true) |
| `plugin.list` | `system.plugin.list` |
| `plugin.cred.*` | `system.plugin.cred.*` |
| `plugin-data.get` | `system.plugin.data.get` |
| `plugin-data.set` | `system.plugin.data.set` |
| `mcp.create` | `system.mcp.create` |
| `mcp.delete` | `system.mcp.delete` |
| `mcp.list` | `system.mcp.list` |
| `mcp.mapping.*` | `system.mcp.mapping.*` |
| `mcp.sync` | `system.mcp.sync` |
| `mcp-federation.list-tools` | `system.mcp.federation.list` |
| `mcp-federation.run` | `system.mcp.federation.run` |
| `sweep.organization` | `system.sweep.organization` |
| `infra.service.detail` | `system.infra.service.detail` |
| `infra.service.list` | `system.infra.service.list` |
| `infra.test.create` | `system.infra.test.create` |

### `namespace.*` (absorbs: profile, doc-root, doc-pattern, docs, projects)

| Current | Target |
|---------|--------|
| `profile.create` | `namespace.create` |
| `profile.delete` | `namespace.delete` |
| `profile.list` | `namespace.list` |
| `profile.detail` | `namespace.detail` |
| `profile.current` | `namespace.current` |
| `profile.use` | `namespace.use` |
| `profile.share.*` | `namespace.share.*` |
| `doc-root.create` | `namespace.doc.root.create` |
| `doc-root.delete` | `namespace.doc.root.delete` |
| `doc-root.list` | `namespace.doc.root.list` |
| `doc-pattern.create` | `namespace.doc.pattern.create` |
| `doc-pattern.delete` | `namespace.doc.pattern.delete` |
| `doc-pattern.list` | `namespace.doc.pattern.list` |
| `docs.read` | `namespace.doc.read` |
| `docs.search` | `namespace.doc.search` |
| `docs.tree` | `namespace.doc.tree` |
| `docs.full-tree` | `namespace.doc.tree.full` |
| `docs.list-roots` | `namespace.doc.root.list` (merge) |
| `docs.list-commands` | `namespace.doc.commands` |
| `projects.list` | `namespace.project.list` |

### `filesystem.*` (no change — already correct prefix)

Currently no `filesystem.*` tools exist yet — this is where NFS/SMB/sync tools land when built.

### `docker.*` (absorbs: docker, docker-runtime)

| Current | Target |
|---------|--------|
| `docker.engine.detail` | `docker.engine.detail` ✓ |
| `docker.engine.update` | `docker.engine.update` ✓ |
| `docker.service.*` | `docker.service.*` ✓ |
| `docker-runtime.create` | `docker.runtime.create` |
| `docker-runtime.delete` | `docker.runtime.delete` |
| `docker-runtime.list` | `docker.runtime.list` |

### `proxmox.*` (absorbs: proxmox, proxmox-endpoint)

| Current | Target |
|---------|--------|
| `proxmox.container.*` | `proxmox.container.*` ✓ |
| `proxmox.node.list` | `proxmox.node.list` ✓ |
| `proxmox.vm.*` | `proxmox.vm.*` ✓ |
| `proxmox-endpoint.create` | `proxmox.endpoint.create` |
| `proxmox-endpoint.delete` | `proxmox.endpoint.delete` |
| `proxmox-endpoint.list` | `proxmox.endpoint.list` |

### `ha.*` (absorbs: ha, ha-endpoint)

| Current | Target |
|---------|--------|
| `ha.automation.list` | `ha.automation.list` ✓ |
| `ha.entity.detail` | `ha.entity.detail` ✓ |
| `ha.entity.list` | `ha.entity.list` ✓ |
| `ha.service.update` | `ha.service.update` ✓ |
| `ha-endpoint.create` | `ha.endpoint.create` |
| `ha-endpoint.delete` | `ha.endpoint.delete` |
| `ha-endpoint.list` | `ha.endpoint.list` |

### `llm.*` (absorbs: engine partially — LLM inference engines)

LLM-specific engines split from `system.engine.*`. When the LLM crate is built, `llm.model.*`, `llm.backend.*` etc. live here.

### `network.*` (new — not yet built)

`network.router.*`, `network.switch.*`, `network.dns.*`, `network.proxy.*`, `network.wifi.*` — all future integrations.

### `db.*` / `schema.*` / `spec.*` — collapse into `namespace`

| Current | Target |
|---------|--------|
| `schema.create` | `namespace.schema.create` |
| `schema.delete` | `namespace.schema.delete` |
| `schema.list` | `namespace.schema.list` |
| `schema-view.detail` | `namespace.schema.view.detail` |
| `schema-view.list` | `namespace.schema.view.list` |
| `spec.create` | `namespace.spec.create` |
| `spec.delete` | `namespace.spec.delete` |
| `spec.detail` | `namespace.spec.detail` |
| `spec.graphql.*` | `namespace.spec.graphql.*` |
| `spec.list` | `namespace.spec.list` |
| `spec.list-db` | `namespace.spec.list-db` |
| `spec.refresh` | `namespace.spec.refresh` |
| `spec.sync-mcp` | `namespace.spec.sync-mcp` |

---

## Summary — Prefix Count Reduction

| Before | After |
|--------|-------|
| agent-backend | system.agent.backend |
| agents | system.agent |
| auth | system.auth |
| config | system.config |
| db | system.db |
| doc-pattern | namespace.doc.pattern |
| doc-root | namespace.doc.root |
| docker | docker ✓ |
| docker-runtime | docker.runtime |
| docs | namespace.doc |
| engine | system.engine |
| ha | ha ✓ |
| ha-endpoint | ha.endpoint |
| host | system.host |
| host_status | system.host.status |
| infra | system.infra |
| mcp | system.mcp |
| mcp-federation | system.mcp.federation |
| pki | system.pki |
| plugin | system.plugin |
| plugin-data | system.plugin.data |
| pod | system.peer (mesh tools) |
| profile | namespace |
| projects | namespace.project |
| proxmox | proxmox ✓ |
| proxmox-endpoint | proxmox.endpoint |
| schedule | system.schedule |
| schema | namespace.schema |
| schema-view | namespace.schema.view |
| secret | system.secret |
| spec | namespace.spec |
| sweep | system.sweep |
| system | system ✓ |

**34 prefixes → 8 top-level domains**

---

## Execution Order

1. **Easy wins** (hyphen → dot, no logic change):
   - `docker-runtime.*` → `docker.runtime.*`
   - `proxmox-endpoint.*` → `proxmox.endpoint.*`
   - `ha-endpoint.*` → `ha.endpoint.*`
   - `host_status.*` → `system.host.status.*`

2. **Merge into `system.*`** (rename only, one domain):
   - `config.*`, `auth.*`, `host.*`, `pki.*`, `db.*`, `schedule.*`, `secret.*`
   - `pod.*` → `system.peer.*`
   - `agent-backend.*`, `agents.*` → `system.agent.*`
   - `engine.*`, `plugin.*`, `plugin-data.*`, `mcp.*`, `mcp-federation.*`
   - `sweep.*`, `infra.*`

3. **Merge into `namespace.*`**:
   - `profile.*` → `namespace.*`
   - `doc-root.*`, `doc-pattern.*`, `docs.*` → `namespace.doc.*`
   - `schema.*`, `schema-view.*` → `namespace.schema.*`
   - `spec.*` → `namespace.spec.*`
   - `projects.*` → `namespace.project.*`

4. **CRUD verb cleanup** (alongside renames):
   - `engine.enable` / `engine.disable` → `system.engine.update { enabled: bool }`
   - `plugin.enable` / `plugin.disable` → `system.plugin.update { enabled: bool }`

---

## Backward Compatibility

Aliases (old name → new name) kept for one release cycle, then removed.
MCP clients that hardcode tool names must update. CLI aliases with deprecation warnings for one release.
