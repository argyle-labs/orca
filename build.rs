use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let agents_dir = env::var("BRAIN_AGENTS_DIR")
        .unwrap_or_else(|_| {
            let home = env::var("HOME").expect("HOME not set");
            format!("{home}/brain/ai/claude/agents")
        });

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = Path::new(&out_dir).join("embedded_agents.rs");

    let mut code = String::from("/// Agent prompts embedded at build time.\n");
    code.push_str("pub fn embedded_agent(name: &str) -> Option<&'static str> {\n");
    code.push_str("    match name {\n");

    let agents_path = Path::new(&agents_dir);
    if agents_path.exists() {
        let mut entries: Vec<_> = fs::read_dir(agents_path)
            .expect("failed to read agents dir")
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let abs = path.canonicalize().expect("failed to canonicalize agent path");
            code.push_str(&format!(
                "        \"{name}\" => Some(include_str!(\"{}\")),\n",
                abs.display()
            ));
            // Tell cargo to rebuild if agent files change
            println!("cargo:rerun-if-changed={}", abs.display());
        }
    }

    code.push_str("        _ => None,\n");
    code.push_str("    }\n");
    code.push_str("}\n");

    fs::write(&dest, code).expect("failed to write embedded_agents.rs");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BRAIN_AGENTS_DIR");
}
