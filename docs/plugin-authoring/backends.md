# Backend plugins (secrets / service / storage / backup)

A backend plugin registers a *backend* that core dispatches domain callbacks to,
rather than (only) exposing tools.

## Secrets backend — the hybrid arm

A secrets backend rides the **hybrid `serve_tool_plugin!` arm**: `backends:`
advertises the `BackendDef`, `backend_dispatch:` answers the `{prefix}.{op}`
callbacks (returning `None` falls through to `#[orca_tool]` dispatch). This is
how `onepassword` ships — `argyle-labs/onepassword`, `src/main.rs` is the
authoritative wiring.

```rust
use plugin_toolkit::prelude::*;
use plugin_toolkit::backend_def::secrets_backends_json;
use plugin_toolkit::contract::secrets_backend::RESOLVE_OP; // = "resolve"

const KIND: &str = "onepassword";
const BACKEND_PREFIX: &str = "secrets_backend.__backend.onepassword";

// (1) advertise the backend
fn backends() -> String { secrets_backends_json(KIND, BACKEND_PREFIX) }

// (2) answer the `resolve` op: {"ref_path": "op://Vault/item/field"} -> value.
//     Return None to fall through to #[orca_tool] dispatch. (Exact match string
//     + arg parsing: see onepassword src/main.rs.)
fn backend_dispatch(op: &str, args_json: &str) -> Option<Result<String, String>> {
    op.ends_with(RESOLVE_OP).then(|| resolve(args_json))
}

plugin_toolkit::serve_tool_plugin! {
    name: "onepassword", target_compat: "",
    backends: backends(),
    backend_dispatch: backend_dispatch,
}
```

Contract + def builders:
- `secrets_backend_def` / `secrets_backends_json` —
  [`../../projects/plugin-toolkit/src/backend_def.rs:265`](../../projects/plugin-toolkit/src/backend_def.rs)
- `SecretsBackend` trait + `RESOLVE_OP` —
  [`../../projects/contract/src/secrets_backend.rs`](../../projects/contract/src/secrets_backend.rs)

`backend_def.rs` also exposes the same one-line def/`_backends_json` pair for the
`topology`, `host_facts`, and `service_identity` domains — advertise the def and
answer its one callback op.

## Service / storage / backup — dedicated macros

These have their own serve macros, each taking a typed `backend:` that implements
the matching `contract` trait (see
[`../../projects/plugin-toolkit/src/serve_macros.rs`](../../projects/plugin-toolkit/src/serve_macros.rs)):

```rust
plugin_toolkit::serve_service_plugin! { name: "audiobookshelf", target_compat: "any", backend: AudiobookshelfBackend::new("audiobookshelf") }
plugin_toolkit::serve_storage_plugin! { name: "smb",            target_compat: "any", backend: SmbBackend::new("smb") }
```

Worked examples: `argyle-labs/audiobookshelf` (service), `argyle-labs/smb`
(storage).
