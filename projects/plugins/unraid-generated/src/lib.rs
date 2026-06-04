//! Typed Unraid GraphQL client modules — one per committed schema.
//!
//! The set of supported versions is whatever ships under
//! `projects/plugins/unraid/schemas/<version>.introspection.json`. The
//! build script discovers them on disk, runs codegen, and exposes a
//! [`SUPPORTED_VERSIONS`] table mapping the wire version string
//! (e.g. `"7.3.1"`) to the generated module name (`"v7_3_1"`).
//!
//! Adding a new schema is purely additive: drop the file in `schemas/`,
//! rebuild, and a new module appears.

include!(concat!(env!("OUT_DIR"), "/modules.rs"));
