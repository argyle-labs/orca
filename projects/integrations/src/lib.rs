//! First-party integrations with external systems. Facade crate — each
//! backend is a standalone workspace member that builds and caches
//! independently. Hard rule: every new integration is its own crate.

pub use orca_integration_docker as docker;
pub use orca_integration_dockge as dockge;
pub use orca_integration_homeassistant as homeassistant;
pub use orca_integration_nfs as nfs;
pub use orca_integration_ntfy as ntfy;
pub use orca_integration_proxmox as proxmox;
pub use orca_integration_smb as smb;
pub use orca_integration_unraid as unraid;
