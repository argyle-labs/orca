//! Every repo path named in a comment or doc must exist.
//!
//! #650 catalogued eight places where prose contradicted code, and item 6 of
//! that issue is this class specifically: "paths, files, flags, and URLs named
//! in comments — mechanically checkable". They are worth enforcing rather than
//! auditing once, because a stale pointer is indistinguishable from a live one
//! when you are reading it, and the cost of following one is a wrong mental
//! model of where behaviour lives. Twelve were dangling when this was written.
//!
//! Prose in this repo is load-bearing — several rules live only in comments —
//! so this is the cheap, mechanical half of keeping it honest.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Paths that are allowed not to exist, each for a stated reason. A path is
/// exempt only if it appears here: the point of the test is that the default is
/// "must exist".
const ALLOWED_MISSING: &[(&str, &str)] = &[
    // Plan documents describe files to CREATE. They are proposals, and the
    // paths in them are the proposal's content, not a pointer to code.
    (
        "projects/packages/",
        "planned crate — docs/planned/packages-primitive.md",
    ),
    ("projects/packages/src/lib.rs", "planned crate"),
    ("projects/packages/src/package_converge.rs", "planned crate"),
    ("projects/db/src/packages.rs", "planned table module"),
    (
        "projects/db/src/secrets.rs",
        "planned; the live one is projects/auth/src/secrets.rs",
    ),
    // Deliberate references to something that was REMOVED — the prose is about
    // its absence, so the path must stay spelled out.
    (
        "projects/plugins",
        "removed; docs/plugin-loading.md documents the removal",
    ),
    (
        "projects/plugins/",
        "removed; docs/CAPABILITY-REGISTRIES.md documents the removal",
    ),
];

fn repo_root() -> PathBuf {
    // <root>/projects/inventory-tests -> <root>
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above projects/inventory-tests")
        .to_path_buf()
}

/// Collect `.rs` and `.md` files, skipping build output and vendored trees.
fn source_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                if matches!(
                    name.as_ref(),
                    "target" | "node_modules" | ".git" | "dist" | "build"
                ) {
                    continue;
                }
                walk(&p, out);
            } else if matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("rs") | Some("md")
            ) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for top in ["projects", "docs"] {
        walk(&root.join(top), &mut out);
    }
    for f in ["README.md", "CRATE_RESPONSIBILITIES.md"] {
        let p = root.join(f);
        if p.exists() {
            out.push(p);
        }
    }
    out
}

/// Pull `` `projects/...` `` style paths out of a file's text.
///
/// Backtick-delimited only, and only for directories the repo actually owns.
/// A bare unquoted word is too easy to confuse with prose; a backticked
/// `projects/...` is unambiguously a pointer at this tree.
fn referenced_paths(text: &str) -> BTreeSet<String> {
    const ROOTS: &[&str] = &["projects/", "docs/", "hooks/", ".cargo/", ".github/"];
    let mut found = BTreeSet::new();
    for chunk in text.split('`').skip(1).step_by(2) {
        let c = chunk.trim();
        if !ROOTS.iter().any(|r| c.starts_with(r)) {
            continue;
        }
        // A reference stops at whitespace; trailing prose punctuation is not
        // part of the path.
        let c = c.split_whitespace().next().unwrap_or(c);
        let c = c.trim_end_matches([',', '.', ';', ':', ')']);
        // `file.rs::symbol` points at an item inside a file. The file is the
        // checkable part; the symbol after `::` is not a path component.
        let c = c.split("::").next().unwrap_or(c);
        // Shorthand, not a literal path: globs (`projects/*/src`), brace
        // expansion (`src/{a,b}.rs`), ellipsis, and type-parameter text are
        // all ways prose names a SET of files rather than pointing at one.
        if c.is_empty()
            || c.contains('*')
            || c.contains('<')
            || c.contains('{')
            || c.contains('\u{2026}')
        {
            continue;
        }
        found.insert(c.to_string());
    }
    found
}

#[test]
fn every_repo_path_named_in_a_comment_exists() {
    let root = repo_root();
    let allowed: BTreeSet<&str> = ALLOWED_MISSING.iter().map(|(p, _)| *p).collect();

    let mut dangling: Vec<String> = Vec::new();
    for file in source_files(&root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for p in referenced_paths(&text) {
            if allowed.contains(p.as_str()) || root.join(&p).exists() {
                continue;
            }
            let where_ = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            dangling.push(format!("  {p}  (named in {where_})"));
        }
    }
    dangling.sort();
    dangling.dedup();

    assert!(
        dangling.is_empty(),
        "these paths are named in comments/docs but do not exist.\n\
         Point them at the real file, or add them to ALLOWED_MISSING with a \
         reason if they are deliberately describing something planned or \
         removed:\n{}",
        dangling.join("\n")
    );
}

#[test]
fn the_allowlist_itself_stays_honest() {
    // An allowlisted path that has since been CREATED is no longer an
    // exception, and leaving it listed would silently exempt a real path from
    // the check forever.
    let root = repo_root();
    let now_exists: Vec<&str> = ALLOWED_MISSING
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| root.join(p).exists())
        .collect();
    assert!(
        now_exists.is_empty(),
        "these are allowlisted as missing but now exist — drop them from \
         ALLOWED_MISSING so they are checked again: {now_exists:?}"
    );
}

#[test]
fn a_path_reference_is_recognised_only_inside_backticks() {
    let found = referenced_paths("see `projects/db/src/lib.rs` and projects/nope/x.rs");
    assert!(found.contains("projects/db/src/lib.rs"));
    assert_eq!(
        found.len(),
        1,
        "unquoted prose must not be treated as a path"
    );
}

#[test]
fn trailing_prose_punctuation_is_not_part_of_the_path() {
    assert!(referenced_paths("in `projects/db/src/lib.rs`.").contains("projects/db/src/lib.rs"));
    assert!(referenced_paths("(`docs/mesh.md`),").contains("docs/mesh.md"));
    // Globs and type-parameter-ish text are not checkable paths.
    assert!(referenced_paths("`projects/*/src/lib.rs`").is_empty());
    // Brace expansion names a set, not a file.
    assert!(referenced_paths("`projects/system/src/{a,b}.rs`").is_empty());
    // A `file::symbol` pointer is checked as the file it names.
    assert!(
        referenced_paths("`projects/db/src/lib.rs::migrate`").contains("projects/db/src/lib.rs")
    );
}
