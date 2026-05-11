use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");

    // Resolve a real runtime version from git so `orca` reports what it is.
    //   on a clean tag             → "0.0.3-rc.3"
    //   N commits past last tag    → "0.0.3-rc.3-dev+5.g66d2ea6"
    //   working tree dirty         → "...-dev+5.g66d2ea6.dirty"
    //   no git / shallow checkout  → "<CARGO_PKG_VERSION>+unknown"
    let version = resolve_version();
    println!("cargo:rustc-env=ORCA_VERSION={version}");
    // Rerun when HEAD or the working tree changes so the version stays fresh.
    println!("cargo:rerun-if-changed={manifest}/../../.git/HEAD");
    println!("cargo:rerun-if-changed={manifest}/../../.git/index");

    // Embed agent .md prompts (was projects/agents/build.rs).
    let agents_dir =
        env::var("ORCA_AGENTS_DIR").unwrap_or_else(|_| format!("{manifest}/src/agents/agents"));
    write_embedded_map(
        Path::new(&agents_dir),
        Path::new(&out_dir).join("embedded_agents.rs"),
        "embedded_agent",
        "embedded_agent_names",
        "Agent",
    );
    println!("cargo:rerun-if-env-changed=ORCA_AGENTS_DIR");

    // Embed slash-command .md prompts (was projects/commands/build.rs).
    let commands_dir = env::var("ORCA_COMMANDS_DIR")
        .unwrap_or_else(|_| format!("{manifest}/src/commands/commands"));
    write_embedded_map(
        Path::new(&commands_dir),
        Path::new(&out_dir).join("embedded_commands.rs"),
        "embedded_command",
        "embedded_command_names",
        "Slash command",
    );
    println!("cargo:rerun-if-env-changed=ORCA_COMMANDS_DIR");

    // Expose the build target triple to the binary (was previously expected
    // to be supplied externally; emitting it from build.rs keeps `cargo
    // check` working without env setup).
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_string());
    println!("cargo:rustc-env=ORCA_BUILD_TARGET={target}");

    println!("cargo:rerun-if-changed=build.rs");

    // Only ensure frontend/dist exists when the `ui` feature is on — that's
    // the only build configuration where RustEmbed reads from it. Headless
    // builds skip this so they don't touch the frontend tree at all.
    if env::var_os("CARGO_FEATURE_UI").is_some() {
        let dist = Path::new(&manifest).join("../frontend/dist");
        fs::create_dir_all(&dist).expect("failed to create frontend/dist stub");
    }
}

fn resolve_version() -> String {
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    // Try `git describe --tags --always --dirty` — gives us either an exact tag
    // ("v0.0.3-rc.3"), an annotated past-tag string ("v0.0.3-rc.3-5-g66d2ea6"),
    // or just a SHA if no tag is reachable.
    let described = match Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return format!("{cargo_version}+unknown"),
    };

    // Strip the conventional leading 'v' from tags.
    let described = described
        .strip_prefix('v')
        .unwrap_or(&described)
        .to_string();

    // Is HEAD exactly a tag? `git describe --tags --exact-match` succeeds iff yes.
    let exact_tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let dirty = described.ends_with("-dirty");

    if exact_tag && !dirty {
        // Clean release build sitting on the tag.
        return described;
    }

    // Past a tag (or no tag). Rewrite "<tag>-<N>-g<sha>[-dirty]" → "<tag>-dev+<N>.g<sha>[.dirty]"
    // to make the "-dev" intent obvious. Falls back to "<sha>-dev" when there's no tag.
    let stripped = described.trim_end_matches("-dirty");
    let parts: Vec<&str> = stripped.rsplitn(3, '-').collect();
    if parts.len() == 3 && parts[0].starts_with('g') {
        // parts = [g<sha>, <N>, <tag>] (reversed)
        let sha = parts[0];
        let n = parts[1];
        let tag = parts[2];
        let mut s = format!("{tag}-dev+{n}.{sha}");
        if dirty {
            s.push_str(".dirty");
        }
        s
    } else {
        // No tag reachable — `git describe` returned a bare SHA (and maybe -dirty).
        let mut s = format!("{cargo_version}-dev+g{stripped}");
        if dirty {
            s.push_str(".dirty");
        }
        s
    }
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
                Err(_) => continue, // skip broken symlinks
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
