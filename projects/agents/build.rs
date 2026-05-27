//! Embed agent .md prompts at build time. Produces `embedded_agents.rs`
//! in `OUT_DIR` with two functions:
//!   - `embedded_agent(name: &str) -> Option<&'static str>` — lookup
//!   - `embedded_agent_names() -> &'static [&'static str]`  — full list
//!
//! `src/embedded.rs` includes the generated file and exposes the higher-level
//! API (`list_embedded_agents`, `load_agent_prompt_from_dirs`, etc.).
//!
//! Override the source dir with `ORCA_AGENTS_DIR` for hot-reload-style dev.

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
    );
    println!("cargo:rerun-if-env-changed=ORCA_AGENTS_DIR");
    println!("cargo:rerun-if-changed=build.rs");
}

fn write_embedded_map(src_dir: &Path, dest: std::path::PathBuf) {
    let mut code = String::from("/// Agent prompts embedded at build time.\n");
    code.push_str("pub fn embedded_agent(name: &str) -> Option<&'static str> {\n");
    code.push_str("    match name {\n");

    let mut names: Vec<String> = vec![];

    if src_dir.exists() {
        let mut entries: Vec<_> = fs::read_dir(src_dir)
            .expect("failed to read agents dir")
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

    code.push_str("/// All agent names embedded at build time.\n");
    code.push_str("pub fn embedded_agent_names() -> &'static [&'static str] {\n");
    code.push_str("    &[\n");
    for name in &names {
        code.push_str(&format!("        \"{name}\",\n"));
    }
    code.push_str("    ]\n");
    code.push_str("}\n");

    fs::write(&dest, code).expect("failed to write embedded_agents.rs");
}
