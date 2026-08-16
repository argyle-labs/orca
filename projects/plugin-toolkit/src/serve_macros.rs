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

/// Dynamic (subprocess) entry for a **backup-KIND** plugin.
///
/// Emits a `fn main()` that serves the orca socket. `backends()` advertises the
/// `backup_kind` domain for `kind`; proxied ops route through
/// [`backup::dispatch_kind_op`](crate::backup::dispatch_kind_op) against a live
/// [`BackupKindPlugin`](crate::backup::BackupKindPlugin). The dispatch is
/// synchronous (the host proxy offloads heavy ops), so no reactor is entered.
///
/// ```rust,ignore
/// plugin_toolkit::serve_backup_kind_plugin! {
///     name: "proxmox-vm",
///     kind: "vm",
///     target_compat: "any",
///     backend: ProxmoxVmKind::new(),
/// }
/// ```
#[macro_export]
macro_rules! serve_backup_kind_plugin {
    (
        name: $name:literal,
        kind: $kind:literal,
        target_compat: $target_compat:literal,
        backend: $backend:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            const __PREFIX: &str = ::core::concat!("backup_kind.__backend.", $name);
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
                ::core::option::Option::Some($crate::backup::dispatch_kind_op(
                    &backend, op, args_json,
                ))
            }
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec::Vec::new(),
                backends_json: $crate::backend_def::backup_kind_backends_json($kind, __PREFIX),
                schema_json: ::std::string::String::from($crate::backend_def::EMPTY_SCHEMAS),
                backend_dispatch: ::core::option::Option::Some(__dispatch),
            })
        }
    };
}

/// Dynamic (subprocess) entry for a **backup-TARGET** plugin.
///
/// Emits a `fn main()` that serves the orca socket. `backends()` advertises the
/// `backup_target` domain for `kind`; proxied ops route through
/// [`backup::dispatch_target_op`](crate::backup::dispatch_target_op) against a
/// live [`BackupTargetPlugin`](crate::backup::BackupTargetPlugin). The dispatch
/// is synchronous, so no reactor is entered.
///
/// ```rust,ignore
/// plugin_toolkit::serve_backup_target_plugin! {
///     name: "pbs",
///     kind: "pbs",
///     target_compat: "any",
///     backend: PbsTarget::new(),
/// }
/// ```
#[macro_export]
macro_rules! serve_backup_target_plugin {
    (
        name: $name:literal,
        kind: $kind:literal,
        target_compat: $target_compat:literal,
        backend: $backend:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            const __PREFIX: &str = ::core::concat!("backup_target.__backend.", $name);
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
                ::core::option::Option::Some($crate::backup::dispatch_target_op(
                    &backend, op, args_json,
                ))
            }
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec::Vec::new(),
                backends_json: $crate::backend_def::backup_target_backends_json($kind, __PREFIX),
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
/// A **pure tool** plugin is a `[lib] rlib` (holding the `#[orca_tool]`
/// registrations) plus a `[[bin]]` that runs the socket loop. The bin must
/// force-link its lib via the required `link:` field — naming the lib crate
/// ident — or the linker drops the whole rlib (nothing in the bin references
/// it) and every tool registration vanishes: the plugin loads with ZERO tools.
/// The hybrid arm force-links implicitly through its `backends`/`backend_dispatch`
/// expressions, so it needs no `link:`.
///
/// ```rust,ignore
/// // Pure: `link` names this plugin's own lib crate.
/// plugin_toolkit::serve_tool_plugin! {
///     name: "jellyfin", target_compat: "10.8-10.10", link: jellyfin,
/// }
/// plugin_toolkit::serve_tool_plugin! {
///     name: "ntfy", target_compat: "",
///     backends: ntfy_backends_json(),
///     backend_dispatch: ntfy_backend_dispatch,
/// }
/// ```
#[macro_export]
macro_rules! serve_tool_plugin {
    // 0a. Hybrid + declared SQL tables. Same as arm 2, but the plugin also
    //     declares plugin-scoped tables (`schemas:` yields the `schema_json`
    //     core applies at load via `db::plugin_tables`). Placed before the
    //     bare arms so a call carrying `schemas:` matches here, not by falling
    //     through with the field ignored.
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backends: $backends:expr,
        backend_dispatch: $backend_dispatch:expr,
        schemas: $schemas:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec![::std::format!("{}.", $name)],
                backends_json: $backends,
                schema_json: $schemas,
                backend_dispatch: ::core::option::Option::Some($backend_dispatch),
            })
        }
    };

    // 0b. Pure tool surface + declared SQL tables. Same as arm 1, but the
    //     plugin declares plugin-scoped tables through `schemas:`.
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        link: $link:path,
        schemas: $schemas:expr $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            #[allow(unused_imports)]
            use $link as _;
            $crate::serve::serve($crate::serve::PluginSpec {
                name: ::std::string::String::from($name),
                version: ::std::string::String::from(::core::env!("CARGO_PKG_VERSION")),
                prefixes: ::std::vec![::std::format!("{}.", $name)],
                backends_json: ::std::string::String::from($crate::backend_def::EMPTY_BACKENDS),
                schema_json: $schemas,
                backend_dispatch: ::core::option::Option::None,
            })
        }
    };

    // 1. Pure tool surface. `link` is the plugin's own lib crate; the emitted
    //    `use $link as _;` is a crate-level reference that keeps the rlib (and
    //    its `#[orca_tool]` inventory) from being dead-stripped at link time.
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        link: $link:path $(,)?
    ) => {
        fn main() -> $crate::anyhow::Result<()> {
            #[allow(unused_imports)]
            use $link as _;
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
