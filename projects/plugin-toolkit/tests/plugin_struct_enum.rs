//! `#[plugin_struct(args)]` on an enum emits `clap::ValueEnum` (+ serde +
//! schemars) instead of `clap::Args`, so a plugin's arg enums stop hand-writing
//! the verbose 8-line derive. Pins that an `args` enum is CLI-parseable,
//! serde-round-trips, and nests inside an `args` struct.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::clap::ValueEnum;
use plugin_toolkit::prelude::*;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[plugin_struct(args)]
enum Flavor {
    #[default]
    Colima,
    Engine,
}

#[plugin_struct(args)]
struct InstallArgs {
    flavor: Flavor,
    name: String,
}

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
