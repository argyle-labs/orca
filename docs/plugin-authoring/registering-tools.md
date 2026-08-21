# Registering tools

A tool is an `async fn` under `#[orca_tool]`. Its **first** parameter is a typed
args struct, its **second** is `&ToolCtx`, and it returns **`anyhow::Result<T>`**
— a plugin never names a bespoke error type. (The prelude re-exports
`anyhow::Result`; there is no `OrcaError` in the authoring surface.)

```rust
use plugin_toolkit::prelude::*;

#[orca_tool(domain = "jellyfin", verb = "server_info")]
/// Return Jellyfin server identity + version.
pub async fn server_info(_args: ServerInfoArgs, _ctx: &ToolCtx) -> anyhow::Result<ServerInfo> {
    // reach the network via the orca-owned HTTP client — never a linked reqwest
    let resp = plugin_toolkit::client::Client::new().get("https://jellyfin.local/System/Info")?;
    let info: ServerInfo = resp.json()?;
    Ok(info)
}
```

The macro emits an `OrcaTool` impl and an `inventory::submit!` registration named
`jellyfin.server_info`. That signature is compile-pinned by the
[drift anchor](README.md#keeping-this-doc-honest). Worked example: jellyfin's
tool surface (`argyle-labs/jellyfin`, `src/tools.rs`).

## Three surfaces, three mechanisms — do not conflate them

| You want… | Use | Page |
|---|---|---|
| Ad-hoc typed tools (calls, actions, queries) | `#[orca_tool]` (above) | this page |
| A stored `{list,detail,create,update,delete}` resource | `#[endpoint_resource]` | [CRUD resources](endpoint-resource-crud.md) |
| A unit-shaped resource (`orca <kind> <verb>`: containers, VMs) | `contract::unit::UnitProvider` | [Unit-shaped resources](unit-provider.md) |
| A typed backend (secrets/service/storage/backup) | a `BackendDef` + serve macro | [Backend plugins](backends.md) |

CRUD and unit-shaped are **separate surfaces** — `#[endpoint_resource]` is not
for containers/VMs, and unit-shaped resources are not `#[endpoint_resource]`.
