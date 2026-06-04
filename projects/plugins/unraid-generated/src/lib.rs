//! Typed Unraid GraphQL client modules, one per supported Unraid version.
//!
//! Each `v<major>_<minor>_<patch>` module is generated at build time by
//! `build.rs` from the committed introspection JSON in
//! `projects/plugins/unraid/schemas/<version>.introspection.json` plus the
//! `.graphql` query files in `queries/`. Hand-written code in this crate
//! should be limited to module wiring — every type lives in the codegen
//! output.
//!
//! Slice A (current): only `v7_3_1` is wired. Slice B adds runtime version
//! probe + multi-version dispatch.

pub mod v7_3_1 {
    // Custom scalars referenced by the generated code via `super::*`. The
    // Unraid schema returns BigInt + PrefixedID as strings on the wire and
    // DateTime as ISO-8601; we keep them all as String here and let callers
    // parse on demand. Swap for typed wrappers if we ever need stronger
    // guarantees at the type boundary.
    pub type BigInt = String;
    pub type PrefixedID = String;
    pub type DateTime = String;

    // Generated enum variants follow the Unraid schema's SCREAMING_SNAKE
    // naming; suppress style/unused lints rather than rewriting upstream.
    #[allow(non_camel_case_types, unused_imports, dead_code, clippy::all)]
    mod generated {
        use super::{BigInt, DateTime, PrefixedID};
        include!(concat!(env!("OUT_DIR"), "/v7_3_1.rs"));
    }
    pub use generated::*;
}
