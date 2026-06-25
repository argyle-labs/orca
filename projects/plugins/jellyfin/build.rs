//! Generate the typed Jellyfin client from the vendored upstream spec.
//!
//! The published spec (`specs/jellyfin-openapi-12.0.0.json`, OpenAPI 3.0.4,
//! MIT) carries ~300 paths and several hundred `components/schemas`.
//! progenitor emits a Rust type for every schema, so we prune the document to
//! just the endpoints this plugin drives plus their transitive schema closure
//! before codegen. Pruning lives in `plugin_toolkit_build` per
//! [[feedback-plugin-toolkit-is-the-gateway]]; never hand-patch a spec here.
//!
//! The emitted module is named `jellyfin` (not the versioned filename) via
//! `generate_one`.

fn main() {
    let spec = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("specs")
        .join("jellyfin-openapi-12.0.0.json");

    plugin_toolkit_build::openapi::generate_one(
        spec,
        "jellyfin",
        "jellyfin",
        &[
            "/System/Info",
            "/System/Restart",
            "/Sessions",
            "/Library/VirtualFolders",
        ],
    )
    .expect("jellyfin openapi codegen");
}
