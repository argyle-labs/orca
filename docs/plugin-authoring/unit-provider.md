# Unit-shaped resources (`UnitProvider`)

Containers, VMs — anything a user addresses as `orca <kind> <verb>` — are
**unit-shaped**. They are **not** `endpoint_resource!`. You implement the
`contract::unit::UnitProvider` trait and advertise it with the `Plugin`
builder's `.unit(..)` facet.

Core dispatches the six generic verbs (`List` / `Detail` / `Create` / `Update` /
`Delete` / `Upsert`) to your provider; the args carry all domain semantics, so
no domain concept leaks into core. Full type surface and rationale:
[`../MANAGED-UNIT.md`](../MANAGED-UNIT.md) and
[`../../projects/contract/src/unit.rs`](../../projects/contract/src/unit.rs).

Advertise the provider with the builder's `.unit(..)` facet — it emits the
`BackendDef` and routes the domain callbacks through the contract's dispatcher
for you:

```rust
plugin_toolkit::instrument::bootstrap!();
use plugin_toolkit::plugin::Plugin;

fn main() -> plugin_toolkit::anyhow::Result<()> {
    Plugin::named("docker")
        .version(env!("CARGO_PKG_VERSION"))
        .tools(["docker."])
        .unit(docker::registration::unit_provider())
        .serve()
}
```

- The `.unit(provider)` facet method + `unit_backend_def`:
  [`../../projects/plugin-toolkit/src/plugin.rs`](../../projects/plugin-toolkit/src/plugin.rs)
  and [`../../projects/plugin-toolkit/src/backend_def.rs`](../../projects/plugin-toolkit/src/backend_def.rs)
- Worked example: `argyle-labs/docker`, `src/registration.rs` (also `dockge`,
  `proxmox`).
