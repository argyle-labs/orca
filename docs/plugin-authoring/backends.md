# Backend plugins (secrets / service / storage / backup)

A backend plugin registers a *backend* that core dispatches domain callbacks to,
rather than (only) exposing tools.

## Secrets backend — the `.secrets(..)` facet

A secrets backend implements the `SecretsBackend` contract trait — its
`resolve` op maps `{"ref_path": "op://Vault/item/field"}` to a value — and is
wired onto the `Plugin` builder with `.secrets(provider)`. The builder
advertises the `BackendDef` and routes the domain's `{prefix}.{op}` callbacks
through the contract's dispatcher for you. This is how `onepassword` ships —
`argyle-labs/onepassword`, `src/main.rs` is the authoritative wiring.

```rust
plugin_toolkit::instrument::bootstrap!();
use plugin_toolkit::plugin::Plugin;

fn main() -> plugin_toolkit::anyhow::Result<()> {
    Plugin::named("onepassword")
        .version(env!("CARGO_PKG_VERSION"))
        .secrets(onepassword::OnePasswordBackend::new("onepassword"))
        .serve()
}
```

A plugin that also exposes an `#[orca_tool]` surface just adds `.tools([..])`
(plus the force-link `use <crate> as _;`) alongside the facet.

Contract + def builders:
- `secrets_backend_def` / the generic `backends_json(Vec<BackendDef>)` serializer —
  [`../../projects/plugin-toolkit/src/backend_def.rs`](../../projects/plugin-toolkit/src/backend_def.rs)
- `SecretsBackend` trait + `RESOLVE_OP` —
  [`../../projects/contract/src/secrets_backend.rs`](../../projects/contract/src/secrets_backend.rs)
- The `.secrets(..)` facet method —
  [`../../projects/plugin-toolkit/src/plugin.rs`](../../projects/plugin-toolkit/src/plugin.rs)

`backend_def.rs` also exposes the same one-line `*_backend_def` builder for the
`topology`, `host_facts`, and `service_identity` domains; the `Plugin` builder
carries matching `.topology(..)` / `.host_facts(..)` facet methods.

## Service / storage / replication — typed facets

Each of these is a typed facet method on the `Plugin` builder taking a `backend`
that implements the matching `contract` trait (see
[`../../projects/plugin-toolkit/src/plugin.rs`](../../projects/plugin-toolkit/src/plugin.rs)):

```rust
Plugin::named("audiobookshelf")
    .version(env!("CARGO_PKG_VERSION"))
    .service(AudiobookshelfBackend::new("audiobookshelf"))
    .serve()

Plugin::named("smb")
    .version(env!("CARGO_PKG_VERSION"))
    .storage(SmbBackend::new("smb"))
    .serve()
```

Backup-kind / backup-target backends have **no dedicated facet method yet** —
they wire through the generic `.backend(def, dispatcher)` escape hatch on the
builder until typed facets land.

Worked examples: `argyle-labs/audiobookshelf` (service), `argyle-labs/smb`
(storage).
