use std::sync::Arc;

use crate::{RemoteExec, ToolCtx};

#[derive(Clone, Debug)]
pub struct RelayingPeer(pub String);

#[derive(Clone, Debug)]
pub struct CaProven;

#[derive(Clone)]
pub struct RelayState {
    pub peer: String,
    pub ca_proven: bool,
    pub remote_exec: Option<Arc<dyn RemoteExec>>,
}

impl std::fmt::Debug for RelayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayState")
            .field("peer", &self.peer)
            .field("ca_proven", &self.ca_proven)
            .finish_non_exhaustive()
    }
}

impl RelayState {
    pub fn capture(ctx: &ToolCtx) -> Option<Self> {
        let RelayingPeer(peer) = ctx.service::<RelayingPeer>().ok()?;
        Some(Self {
            peer,
            ca_proven: ctx.service::<CaProven>().is_ok(),
            remote_exec: ctx.service::<Arc<dyn RemoteExec>>().ok(),
        })
    }

    pub fn apply(&self, ctx: &mut ToolCtx) {
        ctx.register_service(RelayingPeer(self.peer.clone()));
        if self.ca_proven {
            ctx.register_service(CaProven);
        }
        if let Some(remote) = &self.remote_exec {
            ctx.register_service::<Arc<dyn RemoteExec>>(remote.clone());
        }
    }
}

pub fn custody_cleared(ctx: &ToolCtx) -> bool {
    ctx.service::<RelayingPeer>().is_err() || ctx.service::<CaProven>().is_ok()
}
