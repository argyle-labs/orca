//! Drop guards for the process-global storage registries, so a fake registered
//! by one test is removed even if that test panics and never leaks into
//! registry-listing tests sharing the `cargo test` process.

use plugin_toolkit::storage::{
    GuestMountApplier, StorageBackend, deregister_backend, deregister_guest_applier,
    register_backend, register_guest_applier,
};
use std::sync::Arc;

pub(crate) struct Registered(String);

impl Registered {
    pub(crate) fn new(backend: Arc<dyn StorageBackend>) -> Self {
        let name = backend.name().to_string();
        register_backend(backend);
        Registered(name)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        deregister_backend(&self.0);
    }
}

pub(crate) struct RegisteredGuestApplier(String);

impl RegisteredGuestApplier {
    pub(crate) fn new(applier: Arc<dyn GuestMountApplier>) -> Self {
        let name = applier.name().to_string();
        register_guest_applier(applier);
        RegisteredGuestApplier(name)
    }
}

impl Drop for RegisteredGuestApplier {
    fn drop(&mut self) {
        deregister_guest_applier(&self.0);
    }
}
