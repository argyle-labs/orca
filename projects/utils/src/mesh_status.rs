//! Process-global mesh-listener status.
//!
//! The plugin host (mesh accept loop, default port 12002) lives in the
//! `plugins` crate; consumers that need to surface its bind state
//! (notably `system::system_info::SystemInfoReport`) live elsewhere.
//! This atomic is the shared rendezvous: the host writes `true` after a
//! successful bind and `false` on stop / failure; readers observe.
//!
//! Default is `false` (not yet bound). A snapshot taken before
//! `plugin_host::start` has run reports `mesh_listening = false`, which
//! is the truthful answer.
//!
//! Fix for [[project-system-detail-hides-mesh-bind-failure]].

use std::sync::atomic::{AtomicBool, Ordering};

static LISTENING: AtomicBool = AtomicBool::new(false);

pub fn set_listening(on: bool) {
    LISTENING.store(on, Ordering::Relaxed);
}

pub fn is_listening() -> bool {
    LISTENING.load(Ordering::Relaxed)
}
