//! `#[orca_struct(args)]` on an enum emits `clap::ValueEnum` (+ serde +
//! schemars) instead of `clap::Args`, so a plugin's arg enums stop hand-writing
//! the verbose 8-line derive. Pins that an `args` enum is CLI-parseable,
//! serde-round-trips, and nests inside an `args` struct.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::clap::ValueEnum;
use plugin_toolkit::prelude::*;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[orca_struct(args)]
enum Flavor {
    #[default]
    Colima,
    Engine,
}

#[orca_struct(args)]
struct InstallArgs {
    flavor: Flavor,
    name: String,
}

// Deprecated one-release alias: `#[plugin_struct]` + the `#[plugin(...)]` field
// attribute must keep working so external plugin repos migrate on their own
// cadence. Drop this module when the aliases are removed next release.
//
// The module-level `#![allow(deprecated)]` is required because this
// DELIBERATELY exercises the loud deprecation path: the macro appends a
// `#[deprecated]`-using shim as a SIBLING item (not inside the struct), so the
// allow must cover the whole module, else `-D warnings` turns our own
// intentional smoke test into an error.
mod legacy_alias {
    #![allow(deprecated)]
    use super::*;

    #[plugin_struct]
    #[serde(rename_all = "camelCase")]
    pub struct LegacyAliasArgs {
        #[plugin(rename = "renamedField")]
        pub some_field: String,
    }
}
use legacy_alias::LegacyAliasArgs;

#[test]
fn args_enum_is_clap_value_enum() {
    // ValueEnum parsing — what makes it usable as a CLI choice.
    assert_eq!(Flavor::from_str("colima", true).unwrap(), Flavor::Colima);
    assert_eq!(Flavor::from_str("engine", true).unwrap(), Flavor::Engine);
    assert!(Flavor::from_str("nope", true).is_err());
}

#[test]
fn args_enum_serde_round_trips_through_the_toolkit_serde() {
    // The macro injects serde/schemars but imposes no `rename_all` — the author
    // owns casing — so this round-trips on whatever the derive emits.
    let j = plugin_toolkit::serde_json::to_string(&Flavor::Engine).unwrap();
    let back: Flavor = plugin_toolkit::serde_json::from_str(&j).unwrap();
    assert_eq!(back, Flavor::Engine);
}

#[test]
fn args_struct_embeds_the_args_enum_with_default() {
    // The struct still derives Default (its `args` flavor), and the enum field
    // falls back to the enum's `#[default]`.
    let a = InstallArgs::default();
    assert_eq!(a.flavor, Flavor::Colima);
    assert_eq!(a.name, "");
}

#[test]
fn deprecated_plugin_alias_still_serializes_via_plugin_field_attr() {
    // The `#[plugin(rename = ...)]` field attribute maps through to serde on the
    // deprecated `#[plugin_struct]` alias exactly as `#[orca(...)]` does.
    let v = LegacyAliasArgs {
        some_field: "x".into(),
    };
    let j = plugin_toolkit::serde_json::to_string(&v).unwrap();
    assert!(j.contains("renamedField"), "got {j}");
}
