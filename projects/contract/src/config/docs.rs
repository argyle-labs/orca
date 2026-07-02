//! Embedded agent/system config documents.
//!
//! These were previously read from `~/code/argyle-labs/orca/config/` at runtime, which
//! broke any install that wasn't at that exact path. Now they're compiled
//! into the binary at build time from `projects/config/docs/`.
//!
//! Files:
//!   AGENTS.md, CANONICAL_SOURCES.md, CODING_RULES.md, DELEGATION.md,
//!   FRONTEND.md, MEMORY_SYSTEM.md, PERSONA.md, RULES.md, SEVERITY_RUBRIC.md,
//!   TOOL_RULES.md, plugin-schema.json
//!
//! Lookup is case-insensitive on the basename (`tool_rules`, `TOOL_RULES`,
//! and `Tool_Rules` all resolve to `TOOL_RULES.md`).

#[derive(rust_embed::RustEmbed)]
#[folder = "config-docs"]
struct ConfigDocs;

/// All embedded config doc filenames (e.g. `TOOL_RULES.md`, `plugin-schema.json`).
pub fn list_filenames() -> Vec<String> {
    let mut names: Vec<String> = ConfigDocs::iter().map(|s| s.to_string()).collect();
    names.sort();
    names
}

/// All embedded config doc base names without extension (e.g. `TOOL_RULES`,
/// `plugin-schema`). Useful for the CLI lister.
pub fn list_basenames() -> Vec<String> {
    let mut names: Vec<String> = ConfigDocs::iter()
        .filter_map(|s| {
            let path = std::path::Path::new(s.as_ref());
            path.file_stem().map(|s| s.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Read an embedded config doc by name. Tries (in order):
///   1. `<name>.md`
///   2. `<NAME>.md` (uppercase)
///   3. `<name>.json`
///   4. `<name>` (verbatim — for callers passing the full filename)
pub fn get(name: &str) -> Option<String> {
    let candidates = [
        format!("{name}.md"),
        format!("{}.md", name.to_uppercase()),
        format!("{name}.json"),
        name.to_string(),
    ];
    for candidate in candidates {
        if let Some(file) = ConfigDocs::get(&candidate) {
            return Some(String::from_utf8_lossy(&file.data).into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_filenames_non_empty() {
        let names = list_filenames();
        assert!(!names.is_empty(), "expected embedded config docs");
    }

    #[test]
    fn known_docs_present() {
        // These are the agent-config docs we expect at minimum.
        for required in ["AGENTS", "RULES", "TOOL_RULES", "DELEGATION", "PERSONA"] {
            assert!(
                get(required).is_some(),
                "expected config doc {required} to be embedded"
            );
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let upper = get("TOOL_RULES").unwrap();
        let lower = get("tool_rules").unwrap();
        assert_eq!(upper, lower);
    }

    #[test]
    fn unknown_returns_none() {
        assert!(get("definitely_does_not_exist_xyz").is_none());
    }

    #[test]
    fn json_doc_resolvable() {
        // plugin-schema.json should be reachable by basename.
        assert!(get("plugin-schema").is_some());
    }
}
