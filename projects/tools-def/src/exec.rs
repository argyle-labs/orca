//! `exec.run` — run any allowlisted OrcaTool on a paired peer (or locally)
//! over the pod mTLS channel. Top-level on purpose: exec isn't pod-shaped
//! plumbing, it's a first-class capability that happens to use pod transport.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ExecRunArgs {
    /// Target peer — matches `peer_id`, `hostname`, or `addr` in pod_peers.
    /// `"local"` / `"localhost"` round-trips through this host's loopback,
    /// useful for verifying the allowlist without leaving the box.
    #[cfg_attr(feature = "cli", clap(long))]
    pub peer: String,
    /// Fully-qualified tool name (`<domain>.<verb>`), e.g. `system.status`.
    #[cfg_attr(feature = "cli", clap(long))]
    pub tool: String,
    /// JSON args payload for the tool. Omit for tools with no required
    /// fields — sent as `{}` on the wire.
    #[serde(default)]
    #[cfg_attr(feature = "cli", clap(long))]
    pub args: Option<crate::JsonAny>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ExecRunOutput {
    pub peer: String,
    pub tool: String,
    pub result: crate::JsonAny,
}

/// Dispatch a remote-ok OrcaTool on a paired peer over the pod mTLS channel.
/// The peer enforces its `REMOTE_OK` allowlist; destructive tools stay
/// locally callable only.
#[orca_tool(domain = "exec", verb = "run", remote_ok = false, cli = manual)]
async fn exec_run(
    args: ExecRunArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ExecRunOutput> {
    use crate::pod::native_support;
    let payload = args
        .args
        .map(|j| j.0)
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    let r = native_support::svc(ctx)?
        .exec(&args.peer, &args.tool, payload)
        .await?;
    Ok(ExecRunOutput {
        peer: r.peer,
        tool: r.tool,
        result: r.result,
    })
}
