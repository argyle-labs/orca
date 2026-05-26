//! Plugins domain — plugin registry + plugin runtime KV.
pub mod plugin_runtime;
pub mod plugins;

#[cfg(all(test, feature = "native"))]
pub(crate) mod test_support;
