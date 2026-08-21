# Unit-shaped resources (`UnitProvider`)

Containers, VMs — anything a user addresses as `orca <kind> <verb>` — are
**unit-shaped**. They are **not** `endpoint_resource!`. You implement the
`contract::unit::UnitProvider` trait and advertise it as a backend on the hybrid
`serve_tool_plugin!` arm.

Core dispatches the six generic verbs (`List` / `Detail` / `Create` / `Update` /
`Delete` / `Upsert`) to your provider; the args carry all domain semantics, so
no domain concept leaks into core. Full type surface and rationale:
[`../MANAGED-UNIT.md`](../MANAGED-UNIT.md) and
[`../../projects/contract/src/unit.rs`](../../projects/contract/src/unit.rs).

Advertise the provider with `unit_backends_json`:

```rust
use plugin_toolkit::backend_def::unit_backends_json;

plugin_toolkit::serve_tool_plugin! {
    name: "docker", target_compat: "",
    backends: unit_backends_json(&DockerProvider::new(), "unit.__backend.docker"),
    backend_dispatch: docker_unit_dispatch,
}
```

- `unit_backends_json(&provider, invoke_prefix)`:
  [`../../projects/plugin-toolkit/src/backend_def.rs:200`](../../projects/plugin-toolkit/src/backend_def.rs)
- Worked example: `argyle-labs/docker`, `src/registration.rs` (also `dockge`,
  `proxmox`).
