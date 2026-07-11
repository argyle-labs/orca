//! Generate the (now empty) embedded agent/slash-command lookup tables.
//!
//! Core no longer embeds a base agent roster: the full roster (wolf/otter/… .md,
//! slash-commands, templates) lives in the external `argyle-labs/agents` plugin
//! and is registered at runtime through the `plugin_toolkit::agents` seam. This
//! restores orca's original "core carries no embedded agent fallback" design.
//!
//! The generated tables (`embedded_agent` / `embedded_agent_names` and their
//! command siblings) are still produced so `src/embedded.rs` and `src/commands.rs`
//! keep their `include!` targets and the machinery types compile unchanged — they
//! simply resolve to nothing embedded.

use std::env;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");

    write_empty_map(
        Path::new(&out_dir).join("embedded_agents.rs"),
        "embedded_agent",
        "embedded_agent_names",
        "Agent",
    );
    write_empty_map(
        Path::new(&out_dir).join("embedded_commands.rs"),
        "embedded_command",
        "embedded_command_names",
        "Slash command",
    );

    println!("cargo:rerun-if-changed=build.rs");
}

/// Emit an empty embedded lookup table. Core embeds no roster; the external
/// `argyle-labs/agents` plugin supplies it via the registration seam.
fn write_empty_map(dest: std::path::PathBuf, lookup_fn: &str, names_fn: &str, kind_label: &str) {
    let code = format!(
        "/// {kind_label} prompts embedded at build time (none — supplied by the external plugin).\n\
         pub fn {lookup_fn}(_name: &str) -> Option<&'static str> {{\n\
         \x20   None\n\
         }}\n\n\
         /// All {kind_lower} names embedded at build time (empty — supplied by the external plugin).\n\
         pub fn {names_fn}() -> &'static [&'static str] {{\n\
         \x20   &[]\n\
         }}\n",
        kind_lower = kind_label.to_lowercase(),
    );

    std::fs::write(&dest, code).expect("failed to write embedded map");
}
