use std::sync::OnceLock;

static VERSION: OnceLock<&'static str> = OnceLock::new();

pub fn set_version(version: &'static str) {
    VERSION.set(version).ok();
}

pub fn version() -> &'static str {
    VERSION.get().copied().unwrap_or(env!("CARGO_PKG_VERSION"))
}
