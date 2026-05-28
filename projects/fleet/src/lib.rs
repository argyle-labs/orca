//! Fleet domain — pod/mesh, host, meta. (lifecycle dissolved into `system` crate, slice B3.)

pub mod cli;
pub mod host;
pub mod host_identity;
pub mod host_status;
pub mod host_status_writer;
pub mod infra;
pub mod meta;
pub mod native_support;
pub mod pod;
pub mod pod_native;

#[cfg(test)]
pub(crate) mod test_support;

// Cross-bucket inventory smoke tests moved to projects/inventory-tests/ —
// see that crate's Cargo.toml note for why they can't live here.
