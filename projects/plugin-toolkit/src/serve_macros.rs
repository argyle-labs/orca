//! Dynamic (subprocess) plugin-entry macros: the `serve_*_plugin!` family.
//!
//! Each macro emits a whole `fn main()` that connects the orca-provided socket
//! and runs the [`serve`](crate::serve) loop, deriving `backends()` from the
//! plugin's live backend. A plugin is a `[[bin]]` that names no runtime and
//! keeps only its own domain client.
//!
//! `#[macro_export]` publishes each macro at the crate root, so consumers reach
//! them as `plugin_toolkit::serve_service_plugin!` regardless of this module.

/// Dynamic (subprocess) entry for a **service-backend** plugin.
///
/// Emits a `fn main()` that serves the orca socket. `backends()` is derived from
/// the live backend's own descriptor (never restated); the proxied service ops
/// route through [`service::dispatch_op`](crate::service::dispatch_op) on the
/// shared reactor. The plugin is a `[[bin]]`, owns only its domain client, and
/// names no runtime.
///
/// ```rust,ignore
/// plugin_toolkit::serve_service_plugin! {
///     name: "audiobookshelf",
///     target_compat: "any",
///     backend: AudiobookshelfBackend::new("audiobookshelf"),
/// }
/// ```
#[macro_export]
macro_rules! serve_service_plugin {
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backend: $backend:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            const __PREFIX: &str = ::core::concat!("service.__backend.", $name);
            fn __dispatch(
                tool: &str,
                args_json: &str,
            ) -> ::core::option::Option<
                ::core::result::Result<::std::string::String, ::std::string::String>,
            > {
                let op = tool
                    .strip_prefix(__PREFIX)
                    .and_then(|r| r.strip_prefix('.'))?;
                let backend = $backend;
                ::core::option::Option::Some($crate::reactor::block_on(
                    $crate::service::dispatch_op(&backend, op, args_json),
                ))
            }
            let backend = $backend;
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec::Vec::new(),
                backends_json: $crate::backend_def::service_backends_json(&backend, __PREFIX),
                schema_json: ::std::string::String::from($crate::backend_def::EMPTY_SCHEMAS),
                backend_dispatch: ::core::option::Option::Some(__dispatch),
            })
        }
    };
}

/// Dynamic (subprocess) entry for a **storage-backend** plugin.
///
/// Emits a `fn main()` that serves the orca socket. `backends()` is derived from
/// the live backend's own provider (never restated); the proxied storage ops
/// route through [`storage::dispatch_op`](crate::storage::dispatch_op) on the
/// shared reactor. The plugin is a `[[bin]]`, owns only its domain client, and
/// names no runtime.
///
/// ```rust,ignore
/// plugin_toolkit::serve_storage_plugin! {
///     name: "smb",
///     target_compat: "any",
///     backend: SmbBackend::new("smb"),
/// }
/// ```
#[macro_export]
macro_rules! serve_storage_plugin {
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backend: $backend:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            const __PREFIX: &str = ::core::concat!("storage.__backend.", $name);
            fn __dispatch(
                tool: &str,
                args_json: &str,
            ) -> ::core::option::Option<
                ::core::result::Result<::std::string::String, ::std::string::String>,
            > {
                let op = tool
                    .strip_prefix(__PREFIX)
                    .and_then(|r| r.strip_prefix('.'))?;
                let backend = $backend;
                ::core::option::Option::Some($crate::reactor::block_on(
                    $crate::storage::dispatch_op(&backend, op, args_json),
                ))
            }
            let backend = $backend;
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec::Vec::new(),
                backends_json: $crate::backend_def::storage_backends_json(&backend, __PREFIX),
                schema_json: ::std::string::String::from($crate::backend_def::EMPTY_SCHEMAS),
                backend_dispatch: ::core::option::Option::Some(__dispatch),
            })
        }
    };
}

/// Dynamic (subprocess) entry for a **tool-surface** plugin.
///
/// Emits a `fn main()` that serves the orca socket. Two shapes, by composition:
///
/// 1. **Pure tool** — `{ name, target_compat }`. Manifest is the plugin's own
///    `"{name}."` slice of the linked inventory; `backends`/`schema` empty; no
///    backend dispatch.
/// 2. **Hybrid** — `{ name, target_compat, backends, backend_dispatch }`. A tool
///    plugin that ALSO registers a domain backend. `backends` is a `String`
///    yielding the backends JSON; `backend_dispatch` is a
///    `fn(&str, &str) -> Option<Result<String, String>>` handling the domain's
///    `*.__backend.*` calls (returning `None` to fall through to tool dispatch).
///
/// The plugin is a `[[bin]]`, owns only its domain client, and names no runtime.
///
/// ```rust,ignore
/// plugin_toolkit::serve_tool_plugin! { name: "docker", target_compat: ">=20.10" }
/// plugin_toolkit::serve_tool_plugin! {
///     name: "ntfy", target_compat: "",
///     backends: ntfy_backends_json(),
///     backend_dispatch: ntfy_backend_dispatch,
/// }
/// ```
#[macro_export]
macro_rules! serve_tool_plugin {
    // 1. Pure tool surface.
    (
        name: $name:literal,
        target_compat: $target_compat:literal $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec![::std::format!("{}.", $name)],
                backends_json: ::std::string::String::from($crate::backend_def::EMPTY_BACKENDS),
                schema_json: ::std::string::String::from($crate::backend_def::EMPTY_SCHEMAS),
                backend_dispatch: ::core::option::Option::None,
            })
        }
    };

    // 2. Hybrid: tool surface + a registered domain backend.
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backends: $backends:expr,
        backend_dispatch: $backend_dispatch:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec![::std::format!("{}.", $name)],
                backends_json: $backends,
                schema_json: ::std::string::String::from($crate::backend_def::EMPTY_SCHEMAS),
                backend_dispatch: ::core::option::Option::Some($backend_dispatch),
            })
        }
    };
}
