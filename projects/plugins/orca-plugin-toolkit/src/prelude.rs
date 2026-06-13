//! Prelude — the types every toolkit-using plugin imports with a single
//! `use orca_plugin_toolkit::prelude::*;`.
//!
//! Imports below are what `endpoint_resource!`-expanded code references at
//! plugin scope: serde derives, clap derives, the `#[orca_tool]` attribute,
//! `ToolCtx`, the schemars derive, and `anyhow::Result`. Importing all of
//! them keeps the plugin's hand-written `use` lines down to the
//! plugin-specific upstream client.

pub use anyhow::{Result, anyhow, bail};
pub use async_trait::async_trait;
pub use clap;
pub use contract::{JsonAny, ToolCtx};
pub use derive::{endpoint_resource, orca_tool};
pub use schemars::JsonSchema;
pub use serde::{Deserialize, Serialize};
pub use serde_json;
