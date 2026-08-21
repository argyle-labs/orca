# CRUD resources (`#[endpoint_resource]`)

`#[endpoint_resource]` is a struct **attribute** (not a `!` macro). It generates
the five `{list, detail, create, update, delete}` tools **and** the backing
SQLite table from one struct — CLI / MCP / REST surfaces are automatic.

```rust
use plugin_toolkit::endpoint_resource;

#[endpoint_resource(plugin = "dockge", table = "dockge_endpoints")]
pub struct DockgeEndpoint {
    pub base_url: String,
    pub token: String,   // add #[secret] to encrypt at rest
    pub enabled: bool,
}
```

Generates `dockge.{list,detail,create,update,delete}` over `dockge_endpoints`.
You hand-write only upstream API logic and any surface-extension tools. Every
row carries a built-in `routes` column; `Routes::resolve_reachable` (prelude)
picks a live base URL per instance.

- Attribute + `#[secret]`/`table` options:
  [`../../projects/derive/src/lib.rs:127`](../../projects/derive/src/lib.rs)
- Real usage: [`../../projects/system/src/unit_identity.rs:23`](../../projects/system/src/unit_identity.rs)

> CRUD-shaped stored resources only. Containers/VMs (`orca <kind> <verb>`) are
> [unit-shaped](unit-provider.md), a different surface.
