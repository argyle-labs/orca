//! The plugin gateway — the *only* import line a plugin source file
//! ever writes:
//!
//! ```rust,ignore
//! use plugin_toolkit::prelude::*;
//! ```
//!
//! Per the user's directive (2026-06-13): "if it isn't in the plugin
//! toolkit, treat it as if it doesn't exist from the plugin perspective."
//! This prelude re-exports every type, trait, derive, and macro a plugin
//! author needs to build a tool surface — `ToolCtx`, `JsonAny`, the
//! serde / clap / schemars derives, `anyhow::Result` + macros, the
//! `#[orca_tool]` attribute, the `endpoint_resource!` declarative macro,
//! and the runtime helpers used by macro-emitted code.
//!
//! Plugin source files MUST NOT import directly from `contract`,
//! `dispatch`, `derive`, `schemars`, `serde`, `inventory`, `clap`, `db`,
//! `rusqlite`, `anyhow`, or `async_trait`. If you find yourself reaching
//! past the prelude, the toolkit is missing a primitive — file a fix.
//!
//! (Cargo.toml deps on those crates remain transitionally because
//! `#[orca_tool]`-emitted code references them by absolute path. A future
//! refactor will route those paths through the toolkit too.)

// ── Trait + type anchors plugin tools build against ─────────────────────
pub use contract::{JsonAny, ToolCtx};

// ── Macros emitted into plugin scope ────────────────────────────────────
pub use derive::{endpoint_resource, orca_tool};

// ── serde + schemars + clap derives + their support types ──────────────
pub use clap;
pub use schemars::JsonSchema;
pub use serde::{Deserialize, Serialize};
pub use serde_json;

// ── anyhow result + bail/anyhow macros ─────────────────────────────────
pub use anyhow::{Context, Result, anyhow, bail};

// ── async-trait for hand-written async tools ───────────────────────────
pub use async_trait::async_trait;

// ── Toolkit runtime helpers ────────────────────────────────────────────
pub use crate::runtime;

// ── Ecosystem transport primitives ─────────────────────────────────────
// Plugins reach HTTP / GraphQL / OpenAPI through the toolkit so transport
// bug fixes land once and propagate. After `use plugin_toolkit::prelude::*;`
// these are in scope as `http::Client`, `graphql::Client`, `openapi::parse_str`,
// etc. — never `utils::http::…` or `::graphql::…` directly.
pub use crate::{graphql, http, openapi};
