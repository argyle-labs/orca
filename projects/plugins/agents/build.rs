//! Embed agent and slash-command .md prompts at build time.
//!
//! Produces two files in `OUT_DIR`:
//!   - `embedded_agents.rs`   — `embedded_agent` / `embedded_agent_names`
//!   - `embedded_commands.rs` — `embedded_command` / `embedded_command_names`
//!
//! `src/embedded.rs` includes the generated agent file and exposes the
//! higher-level API. `src/commands.rs` includes the generated command file.
//!
//! Override the source dirs with `ORCA_AGENTS_DIR` / `ORCA_COMMANDS_DIR` for
//! hot-reload-style dev.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");

    let agents_dir =
        env::var("ORCA_AGENTS_DIR").unwrap_or_else(|_| format!("{manifest}/src/agents"));
    write_embedded_map(
        Path::new(&agents_dir),
        Path::new(&out_dir).join("embedded_agents.rs"),
        "embedded_agent",
        "embedded_agent_names",
        "Agent",
    );
    println!("cargo:rerun-if-env-changed=ORCA_AGENTS_DIR");

    let commands_dir =
        env::var("ORCA_COMMANDS_DIR").unwrap_or_else(|_| format!("{manifest}/src/commands"));
    write_embedded_map(
        Path::new(&commands_dir),
        Path::new(&out_dir).join("embedded_commands.rs"),
        "embedded_command",
        "embedded_command_names",
        "Slash command",
    );
    println!("cargo:rerun-if-env-changed=ORCA_COMMANDS_DIR");

    println!("cargo:rerun-if-changed=build.rs");
}

fn write_embedded_map(
    src_dir: &Path,
    dest: std::path::PathBuf,
    lookup_fn: &str,
    names_fn: &str,
    kind_label: &str,
) {
    let mut code = format!("/// {kind_label} prompts embedded at build time.\n");
    code.push_str(&format!(
        "pub fn {lookup_fn}(name: &str) -> Option<&'static str> {{\n"
    ));
    code.push_str("    match name {\n");

    let mut names: Vec<String> = vec![];

    if src_dir.exists() {
        let mut entries: Vec<_> = fs::read_dir(src_dir)
            .expect("failed to read embed dir")
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let Some(stem) = path.file_stem() else {
                continue;
            };
            let name = stem.to_string_lossy().to_string();
            let abs = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            code.push_str(&format!(
                "        \"{name}\" => Some(include_str!(\"{}\")),\n",
                abs.display()
            ));
            println!("cargo:rerun-if-changed={}", abs.display());
            names.push(name);
        }
    }

    code.push_str("        _ => None,\n");
    code.push_str("    }\n");
    code.push_str("}\n\n");

    code.push_str(&format!(
        "/// All {} names embedded at build time.\n",
        kind_label.to_lowercase()
    ));
    code.push_str(&format!(
        "pub fn {names_fn}() -> &'static [&'static str] {{\n"
    ));
    code.push_str("    &[\n");
    for name in &names {
        code.push_str(&format!("        \"{name}\",\n"));
    }
    code.push_str("    ]\n");
    code.push_str("}\n");

    fs::write(&dest, code).expect("failed to write embedded map");
}
