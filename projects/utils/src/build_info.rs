use std::sync::OnceLock;

static VERSION: OnceLock<&'static str> = OnceLock::new();

pub fn set_version(version: &'static str) {
    VERSION.set(version).ok();
}

pub fn version() -> &'static str {
    VERSION.get().copied().unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn base_version() -> &'static str {
    strip_build_metadata(version())
}

fn strip_build_metadata(version: &str) -> &str {
    version
        .split_once("-dev+")
        .map_or(version, |(base, _)| base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_build_metadata_keeps_release_and_prerelease() {
        assert_eq!(strip_build_metadata("0.2.1"), "0.2.1");
        assert_eq!(strip_build_metadata("0.2.1-rc.21"), "0.2.1-rc.21");
        assert_eq!(
            strip_build_metadata("0.2.1-rc.21-dev+gabc1234"),
            "0.2.1-rc.21"
        );
    }
}
