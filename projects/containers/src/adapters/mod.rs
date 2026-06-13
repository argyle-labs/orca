//! Per-runtime adapter implementations of [`crate::RuntimeAdapter`].
//!
//! Each adapter handles one runtime end-to-end (list + inspect + future
//! start/stop/restart/logs in C3). The module mirrors
//! `projects/notifications/`'s "trait + builder registry" shape: callers
//! never branch on `RuntimeKind` — they pull the adapter set from the
//! registry and dispatch through the trait.

pub mod docker;
pub mod lxc_proxmox;
pub mod lxc_proxmox_api;
