//! Every in-repo `projects/…` path (and `:line`) the plugin-authoring docs cite
//! still exists on disk. Lives here rather than in plugin-toolkit because it
//! reads the whole tree, which only this crate's CI hash covers.

/// Every in-repo `projects/…` path (optionally `:line`) the plugin-authoring
/// docs cite must resolve. Cross-repo `argyle-labs/*` references are
/// illustrative and intentionally not checked.
#[test]
fn doc_paths_resolve() {
    use std::path::PathBuf;

    // CARGO_MANIFEST_DIR = <root>/projects/inventory-tests
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above the crate manifest")
        .to_path_buf();

    let docs_dir = repo_root.join("docs/plugin-authoring");
    let mut checked = 0usize;

    for entry in std::fs::read_dir(&docs_dir).expect("docs/plugin-authoring must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap();
        for raw in body.split(|c: char| c.is_whitespace() || "()[]`\"'<>".contains(c)) {
            let tok = raw.trim_start_matches("../").trim_start_matches("../");
            if !tok.starts_with("projects/") {
                continue;
            }
            // Split a trailing `:<line>` if present.
            let (rel, line) = match tok.rsplit_once(':') {
                Some((p, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                    (p, Some(n.parse::<usize>().unwrap()))
                }
                _ => (tok, None),
            };
            if !(rel.ends_with(".rs") || rel.ends_with(".md")) {
                continue;
            }
            let full = repo_root.join(rel);
            assert!(
                full.exists(),
                "{}: cites missing path `{rel}`",
                path.display()
            );
            if let Some(n) = line {
                let count = std::fs::read_to_string(&full).unwrap().lines().count();
                assert!(
                    count >= n,
                    "{}: cites `{rel}:{n}` but that file has only {count} lines",
                    path.display()
                );
            }
            checked += 1;
        }
    }

    assert!(
        checked > 0,
        "expected to path-check at least one projects/ citation"
    );
}
