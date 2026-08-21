# Agents & per-plugin data

## Contributing agents

Agent registration is one plugin capability among many (`agents.register`,
alongside `db.op` / `secret.op` / `http.request`). The core agents domain
(`projects/agents`) holds the registry and surfacing tools; plugins supply the
content — agents, hooks, skills, slash commands, and CLAUDE.md fragments. See
[`../CAPABILITY-REGISTRIES.md`](../CAPABILITY-REGISTRIES.md).

A native plugin registers by calling `plugin_toolkit::agents::register` with an
`AgentRegistration` (from `plugin_toolkit::abi`):

```rust
use plugin_toolkit::agents::register;
use plugin_toolkit::abi::AgentRegistration;

register(AgentRegistration {
    name: "my-plugin".into(),
    agents_json,           // JSON array of agent definitions
    hooks_json,            // JSON array of hooks
    skills_json,           // JSON array of skills
    commands_json,         // JSON array of slash commands
    prompt_fragments_json, // JSON array of CLAUDE.md fragments
})?;
```

This sends the `agents.register` capability; the host routes it into the core
agents domain — the same seam pattern as `db.op` / `secret.op`. Agents surface
through `agent.{list,get,run}` and `orca agents`.

A **manifest plugin** contributes agents declaratively via the `[plugin.agents]`
`manifest_dir` (see [Manifest plugins](manifest-plugins.md)).

## Per-plugin data (`plugin_data`)

Both plugin types can store encrypted per-plugin key/value data — native plugins
via the `db.op` capability, manifest plugins via the REST surface:

```
GET    /api/plugins/{id}/data           → list all entries
GET    /api/plugins/{id}/data/{key}     → get one
PUT    /api/plugins/{id}/data/{key}     → set { "value": "..." }
DELETE /api/plugins/{id}/data/{key}     → delete
```
