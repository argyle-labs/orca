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
    "rusqlite",
    "libsqlite3-sys",
    "mysql_async",
    "lettre",
    "openssl",
    "openssl-sys",
    "blake3",
    "orca-db",
    "orca-containers",
    "orca-notifications",
    "orca-secrets",
];

const PLUGIN_PROFILES: &[&str] = &[
    "tools,db,delegated-http",
    "tools,db,containers,delegated-http",
];

fn forbidden_in(selector: &[&str]) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .arg("tree")
        .args(selector)
        .args(["-e", "normal", "--prefix", "none", "--target", "all"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| FORBIDDEN.contains(name))
        .map(str::to_string)
        .collect()
}

#[test]
fn thin_toolkit_graph_excludes_heavy_crates() {
    let found = forbidden_in(&["-p", env!("CARGO_PKG_NAME")]);
    assert!(found.is_empty(), "thin plugin graph links {found:?}");
}

#[test]
fn plugin_feature_profiles_exclude_heavy_crates() {
    for features in PLUGIN_PROFILES {
        let found = forbidden_in(&[
            "-p",
            "orca-plugin-toolkit",
            "--no-default-features",
            "-F",
            features,
        ]);
        assert!(found.is_empty(), "toolkit [{features}] links {found:?}");
    }
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
