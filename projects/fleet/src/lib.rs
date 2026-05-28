//! Fleet domain — host, infra, meta. (pod/mesh moved to projects/pod/ in
//! slice 4; lifecycle dissolved into `system` in slice B3.) Host* will move
//! to `system` when fleet is dissolved in slices 5/6.

pub mod host;
pub mod host_identity;
pub mod host_status;
pub mod host_status_writer;
pub mod infra;
pub mod meta;

#[cfg(test)]
pub(crate) mod test_support;

// Cross-bucket inventory smoke tests moved to projects/inventory-tests/ —
// see that crate's Cargo.toml note for why they can't live here.
