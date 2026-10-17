use std::process::Command;

const FORBIDDEN: &[&str] = &[
    "reqwest",
    "hyper",
    "rustls",
    "ring",
    "url",
    "tikv-jemalloc-sys",
    "tikv-jemallocator",
    "utoipa",
];

#[test]
fn thin_toolkit_graph_excludes_heavy_crates() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree",
            "-p",
            env!("CARGO_PKG_NAME"),
            "-e",
            "normal",
            "--prefix",
            "none",
            "--target",
            "all",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    let found: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| FORBIDDEN.contains(name))
        .collect();
    assert!(found.is_empty(), "thin plugin graph links {found:?}");
}

#[test]
fn thin_toolkit_graph_has_no_clap_defaults() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree",
            "-p",
            env!("CARGO_PKG_NAME"),
            "-e",
            "features",
            "-i",
            "clap",
            "--prefix",
            "none",
            "--target",
            "all",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    let tree = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tree.contains("clap feature \"default\""),
        "thin plugin graph enables clap defaults"
    );
}
