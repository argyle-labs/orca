//! Infra domain — docker compose service listing, logs, test runner.
pub mod infra;

#[cfg(all(test, feature = "native"))]
pub(crate) mod test_support;
