//! First-party OrcaTool implementations.
//!
//! Every tool here is exposed automatically on four surfaces:
//!   - **MCP** stdio (`tools/list`, `tools/call`)
//!   - **REST** (`POST /api/tools/<name>`)
//!   - **CLI**  (`orca exec <name>`)
//!   - **WASM client** (`orcaClient.<name>(args)` — when the `wasm` feature
//!     is enabled and the crate is consumed from `projects/client/wasm/`).
//!
//! New tools land here; the macro at the bottom of this file is the single
//! enrollment point. Server-side glue is intentionally absent — anything that
//! reaches into server internals stays in `projects/server/src/mcp/` until a
//! service-trait abstraction lets it move across.

// Re-exports used by the `orca_tools!` macro so callers don't need their own
// paths to ToolRegistry.
#[doc(hidden)]
pub mod __private {
    pub use orca_utils::tool::ToolRegistry;
}

pub mod engine;

/// Enroll a set of OrcaTool types as the first-party tool surface.
///
/// Expands to:
///   - `pub fn register_all(reg: &mut ToolRegistry)` — server uses this to
///     populate the runtime registry, which then drives MCP + REST + CLI.
///   - (when `feature = "wasm"`) wasm-bindgen exports — one method per tool
///     on a generated client struct. **Not implemented yet** — placeholder
///     for Phase 3 of the unified-surface plan.
#[macro_export]
macro_rules! orca_tools {
    ( $( $tool:path ),* $(,)? ) => {
        pub fn register_all(reg: &mut $crate::__private::ToolRegistry) {
            $( reg.register::<$tool>(); )*
        }
    };
}

orca_tools! {
    engine::EngineList,
    engine::EngineAdd,
    engine::EngineRemove,
    engine::EngineEnable,
    engine::EngineDisable,
}
