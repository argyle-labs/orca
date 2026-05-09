use orca_sdk::manifest::{Manifest, parse_path};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: validate <plugins-dir-or-manifest> [more...]");
        return ExitCode::from(2);
    }

    let mut manifests: Vec<PathBuf> = Vec::new();
    for arg in &args {
        let p = PathBuf::from(arg);
        if p.is_dir() {
            for entry in p.read_dir().expect("read_dir") {
                let entry = entry.expect("entry");
                let path = entry.path();
                if path.is_dir() {
                    let m = path.join(Manifest::FILENAME);
                    if m.is_file() {
                        manifests.push(m);
                    }
                }
            }
        } else if p.is_file() {
            manifests.push(p);
        }
    }
    manifests.sort();

    let mut failed = 0usize;
    for path in &manifests {
        match parse_path(path) {
            Ok(m) => {
                println!(
                    "OK   {:<14} v{:<8} caps={} deps={} eager={} surfaces=[{}]",
                    m.plugin.id,
                    m.plugin.version,
                    m.capabilities.len(),
                    m.depends_on.len(),
                    m.runtime.eager,
                    surfaces_str(&m.surfaces),
                );
            }
            Err(e) => {
                failed += 1;
                println!("FAIL {} — {:#}", path.display(), e);
            }
        }
    }

    println!("\n{}/{} manifests valid", manifests.len() - failed, manifests.len());
    if failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

fn surfaces_str(s: &orca_sdk::manifest::SurfacesSection) -> String {
    let mut out = Vec::new();
    if s.mcp { out.push("mcp"); }
    if s.cli { out.push("cli"); }
    if s.ui { out.push("ui"); }
    if s.docs { out.push("docs"); }
    if s.jobs { out.push("jobs"); }
    if s.storage { out.push("storage"); }
    if s.federation { out.push("federation"); }
    out.join(",")
}
