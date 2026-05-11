//! First-party integrations with external systems (Docker, Proxmox,
//! Home Assistant, etc.). Each subsystem lives in its own module; the
//! crate aggregates them so consumers (the runtime, MCP handlers) only
//! pull a single workspace dependency.

pub mod docker;
pub mod dockge;
pub mod homeassistant;
pub mod nfs;
pub mod ntfy;
pub mod proxmox;
pub mod smb;
pub mod unraid;
