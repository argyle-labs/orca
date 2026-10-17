use std::fmt;

use tracing::warn;
use utils::jsonrpc::ErrorObject;

#[derive(Debug)]
pub struct PeerRefusal(pub String);

impl fmt::Display for PeerRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PeerRefusal {}

macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err(anyhow::Error::new($crate::mesh::refusal::PeerRefusal(format!($($arg)*))))
    };
}

macro_rules! refuse_unless {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            $crate::mesh::refusal::refuse!($($arg)*);
        }
    };
}

pub(crate) use refuse;
pub(crate) use refuse_unless;

pub(crate) fn peer_facing_message(method: &str, e: &anyhow::Error) -> String {
    if let Some(refusal) = e.downcast_ref::<PeerRefusal>() {
        return refusal.0.clone();
    }
    warn!("[mesh] {method} failed: {e:#}");
    format!("{method} failed")
}

pub(crate) fn peer_facing_error(method: &str, e: &anyhow::Error) -> ErrorObject {
    ErrorObject::internal(&peer_facing_message(method, e))
}
