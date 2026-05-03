use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let agents_dir = env::var("ORCA_AGENTS_DIR").unwrap_or_else(|_| {
        let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        format!("{manifest}/../agents")
    });

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = Path::new(&out_dir).join("embedded_agents.rs");

    let mut code = String::from("/// Agent prompts embedded at build time.\n");
    code.push_str("pub fn embedded_agent(name: &str) -> Option<&'static str> {\n");
    code.push_str("    match name {\n");

    let agents_path = Path::new(&agents_dir);
    let mut names: Vec<String> = vec![];

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
            // Skip broken symlinks (e.g. macOS project agents on Linux)
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

    // Generate a companion function that lists all embedded agent names.
    code.push_str("/// All agent names embedded at build time.\n");
    code.push_str("pub fn embedded_agent_names() -> &'static [&'static str] {\n");
    code.push_str("    &[\n");
    for name in &names {
        code.push_str(&format!("        \"{name}\",\n"));
    }
    code.push_str("    ]\n");
    code.push_str("}\n");

    fs::write(&dest, code).expect("failed to write embedded_agents.rs");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORCA_AGENTS_DIR");

    // Ensure frontend/dist exists so RustEmbed doesn't fail before the frontend
    // is built. Dev mode (--dev flag) skips the static handler at runtime, so
    // this stub is never served. Release builds run `make build` which populates
    // dist/ with real assets before the final cargo build.
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let dist = Path::new(&manifest).join("../frontend/dist");
    fs::create_dir_all(&dist).expect("failed to create frontend/dist stub");
}
