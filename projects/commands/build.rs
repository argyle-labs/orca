use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let commands_dir = env::var("BRAIN_COMMANDS_DIR").unwrap_or_else(|_| {
        let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        format!("{manifest}/src/commands")
    });

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = Path::new(&out_dir).join("embedded_commands.rs");

    let mut code = String::from("/// Slash command prompts embedded at build time.\n");
    code.push_str("pub fn embedded_command(name: &str) -> Option<&'static str> {\n");
    code.push_str("    match name {\n");

    let commands_path = Path::new(&commands_dir);
    let mut names: Vec<String> = vec![];

    if commands_path.exists() {
        let mut entries: Vec<_> = fs::read_dir(commands_path)
            .expect("failed to read commands dir")
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
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
    code.push_str("/// All command names embedded at build time.\n");
    code.push_str("pub fn embedded_command_names() -> &'static [&'static str] {\n");
    code.push_str("    &[\n");
    for name in &names {
        code.push_str(&format!("        \"{name}\",\n"));
    }
    code.push_str("    ]\n");
    code.push_str("}\n");

    fs::write(&dest, code).expect("failed to write embedded_commands.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BRAIN_COMMANDS_DIR");

    // Expose the Rust target triple as a compile-time constant for self-update.
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=BRAIN_BUILD_TARGET={target}");
}
