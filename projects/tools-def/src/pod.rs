//! Pod / mesh tools surfaced to the four-surface registry.
//!
//! `pod.list` mirrors the CLI's `orca pod list` so the web overview can
//! render paired peers without a bespoke REST endpoint.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PodPeerDto {
    pub peer_id: String,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub last_seen_at: i64,
    pub local_secure: bool,
    pub peer_secure: bool,
    /// "active" | "departed".
    pub status: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PodPeerList(pub Vec<PodPeerDto>);

pub struct PodList;
impl OrcaToolDef for PodList {
    const NAME: &'static str = "pod.list";
    const DESCRIPTION: &'static str = "List paired pod peers (mesh members).";
    type Args = EmptyArgs;
    type Output = PodPeerList;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_db as db;
    use orca_utils::tool::{OrcaTool, ToolCtx};

    impl From<db::pod::PeerSummary> for PodPeerDto {
        fn from(p: db::pod::PeerSummary) -> Self {
            Self {
                peer_id: p.peer_id,
                hostname: p.hostname,
                addr: p.addr,
                port: p.port,
                last_seen_at: p.last_seen_at,
                local_secure: p.local_secure,
                peer_secure: p.peer_secure,
                status: p.status,
            }
        }
    }

    #[async_trait]
    impl OrcaTool for PodList {
        async fn run(_args: EmptyArgs, _ctx: &ToolCtx) -> Result<PodPeerList> {
            let conn = db::open_default()?;
            Ok(PodPeerList(
                db::pod::list_peers(&conn)?
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ))
        }
    }
}
