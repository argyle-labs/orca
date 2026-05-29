//! Runner trait — implementations carry the async `run` body.
//! See `OrcaToolDef` for the metadata supertrait.
//!
//! # Implementing
//!
//! ```rust,ignore
//! #[derive(Deserialize, JsonSchema)]
//! pub struct Args { pub mode: String }
//!
//! #[derive(Serialize, JsonSchema)]
//! pub struct Output { pub mode: String, pub applied: bool }
//!
//! pub struct MyTool;
//!
//! impl OrcaToolDef for MyTool {
//!     const NAME: &'static str = "my_tool";
//!     const DESCRIPTION: &'static str = "Does the thing.";
//!     type Args = Args;
//!     type Output = Output;
//! }
//!
//! #[async_trait]
//! impl OrcaTool for MyTool {
//!     async fn run(args: Args, _ctx: &ToolCtx) -> Result<Output> {
//!         Ok(Output { mode: args.mode, applied: true })
//!     }
//! }
//! ```

use crate::{OrcaToolDef, ToolCtx};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait OrcaTool: OrcaToolDef {
    async fn run(args: Self::Args, ctx: &ToolCtx) -> Result<Self::Output>;
}
