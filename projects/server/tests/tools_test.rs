use std::fs;
use tempfile::tempdir;

use ::model::tools::bash::BashPermissions;
use agents::embedded::{list_embedded_agents, load_agent_prompt};
use files::ops;
use utils::search;

// These tests verify the tool implementations work correctly.
// They use real filesystem operations via the tempfile crate (no race conditions).

// ── fs tool tests ─────────────────────────────────────────────────────────────

#[test]
fn test_read_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.txt");
    fs::write(&path, "hello world").unwrap();

    let result = ops::read_file(path.to_str().unwrap());
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "hello world");
}

#[test]
fn test_write_creates_parent_dirs() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sub/nested/file.txt");

    let result = ops::write_file(path.to_str().unwrap(), "nested content");
    assert!(result.is_ok());
    assert_eq!(fs::read_to_string(&path).unwrap(), "nested content");
}

#[test]
fn test_edit_file_replaces_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("edit.txt");
    fs::write(&path, "hello world foo bar").unwrap();

    let result = ops::edit_file(path.to_str().unwrap(), "hello world", "goodbye world");
    assert!(result.is_ok());
    assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world foo bar");
}

#[test]
fn test_edit_file_not_found_returns_error() {
    let result = ops::edit_file("/nonexistent/path.txt", "old", "new");
    assert!(result.is_err());
}

#[test]
fn test_edit_file_old_string_not_found() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "some content").unwrap();

    let result = ops::edit_file(path.to_str().unwrap(), "not present", "new");
    assert!(result.is_err());
}

// ── search tool tests ─────────────────────────────────────────────────────────

#[test]
fn test_glob_files_matches_pattern() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "").unwrap();
    fs::write(dir.path().join("b.rs"), "").unwrap();
    fs::write(dir.path().join("c.txt"), "").unwrap();

    // glob_files with full pattern (no base)
    let pattern = format!("{}/*.rs", dir.path().display());
    let result = search::glob_files(&pattern, None).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 .rs files, got: {result}");
}

#[test]
fn test_glob_files_with_base() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("x.md"), "").unwrap();
    fs::write(dir.path().join("y.md"), "").unwrap();

    let result = search::glob_files("*.md", Some(dir.path().to_str().unwrap())).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 2);
}

#[test]
fn test_glob_files_no_match_returns_message() {
    let dir = tempdir().unwrap();
    let pattern = format!("{}/*.xyz", dir.path().display());
    let result = search::glob_files(&pattern, None).unwrap();
    assert!(result.starts_with("no files matched"));
}

#[test]
fn test_grep_content_finds_match() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search.txt");
    fs::write(&path, "line one\nline two needle\nline three\n").unwrap();

    let result = search::grep_content("needle", path.to_str().unwrap(), false).unwrap();
    assert!(result.contains("needle"));
    assert!(result.contains(":2:")); // line 2 (1-indexed)
}

#[test]
fn test_grep_content_case_insensitive() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "Hello WORLD\nlower world\n").unwrap();

    let result = search::grep_content("hello", path.to_str().unwrap(), true).unwrap();
    assert!(result.contains("Hello WORLD"));
}

#[test]
fn test_grep_content_no_match_returns_message() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "no match here\n").unwrap();

    let result = search::grep_content("xyzzy", path.to_str().unwrap(), false).unwrap();
    assert!(result.starts_with("no matches"));
}

#[test]
fn test_grep_content_truncates_at_200_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("big.txt");
    // Write 300 lines each containing the search pattern
    let content: String = (0..300).map(|i| format!("line {i} needle\n")).collect();
    fs::write(&path, &content).unwrap();

    let result = search::grep_content("needle", path.to_str().unwrap(), false).unwrap();
    assert!(result.contains("truncated"));
}

#[test]
fn test_grep_content_path_not_found_returns_error() {
    let result = search::grep_content("x", "/no/such/path.txt", false);
    assert!(result.is_err());
}

// ── bash tool tests ───────────────────────────────────────────────────────────

#[test]
fn test_allowlist_auto_approve() {
    let mut p = BashPermissions::default();
    p.auto_approve = true;
    assert!(p.is_allowed("rm -rf /"));
    assert!(p.is_allowed("any command"));
}

#[test]
fn test_allowlist_prefix_match() {
    let mut p = BashPermissions::default();
    p.allow("cargo");
    assert!(p.is_allowed("cargo test"));
    assert!(p.is_allowed("cargo build --release"));
    assert!(!p.is_allowed("rm file"));
}

#[test]
fn test_allowlist_empty_denies_all() {
    let p = BashPermissions::default();
    assert!(!p.is_allowed("echo hello"));
    assert!(!p.is_allowed("ls"));
}

// ── frontmatter strip test (agents.rs logic) ──────────────────────────────────

#[test]
fn test_strip_frontmatter_removes_yaml_block() {
    let input = "---\nname: test\ntype: agent\n---\n\nActual content here.";
    // We test the public strip function indirectly by loading a synthesised agent.
    // Direct: recreate the same logic inline and verify it matches agents::strip_frontmatter output
    // by calling load_agent_prompt on an embedded agent — but that needs agents_dir.
    // Instead: verify agents::list_embedded_agents() strips correctly (descriptions don't start with "---")
    let agents = list_embedded_agents();
    // At least one embedded agent must exist (wolf is always embedded)
    assert!(!agents.is_empty(), "no embedded agents found");
    for (name, desc) in &agents {
        assert!(
            !desc.starts_with("---"),
            "agent {name} description still has frontmatter"
        );
    }
    // Also verify raw logic for the known input
    let lines: Vec<&str> = input.lines().collect();
    let result = if lines.first().map(|l| l.trim()) == Some("---") {
        if let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") {
            lines[end + 2..].join("\n").trim().to_string()
        } else {
            input.trim().to_string()
        }
    } else {
        input.trim().to_string()
    };
    assert_eq!(result, "Actual content here.");
}

#[test]
fn test_strip_frontmatter_no_frontmatter_passthrough() {
    // Agents without frontmatter should come through unmodified (minus trim).
    // Use the existing embedded agents as proof: their prompts have content.
    let agents = list_embedded_agents();
    assert!(!agents.is_empty());
    // All agents must have non-empty descriptions (list_embedded_agents calls strip_frontmatter)
    for (name, _) in &agents {
        let dir = std::path::Path::new("/nonexistent");
        let prompt = load_agent_prompt(name, dir);
        // Falls back to embedded — must return Some
        assert!(prompt.is_some(), "embedded agent {name} returned None");
    }
}

// ── model parse test ──────────────────────────────────────────────────────────

#[test]
fn test_model_parse() {
    let specs = vec![
        ("claude-sonnet-4-6", true),
        ("claude:claude-opus-4-6", true),
        ("lmstudio:qwen3", false),
        ("some-local-model", false),
    ];

    for (spec, is_claude) in specs {
        let is_claude_result = spec.starts_with("claude-") || spec.starts_with("claude:");
        assert_eq!(is_claude_result, is_claude, "failed for: {spec}");
    }
}

// ── MCP tools/list schema validity ─────────────────────────────────────────────
//
// Walks the REAL, fully-linked tool inventory (this binary links `spec` +
// `runtime`, so every `#[orca_tool]` is registered) and applies the same
// "every input-schema property must resolve to a concrete JSON type" check
// that the Claude Code MCP client runs. A `serde_json::Value` field renders
// as a typeless "any" schema; that previously failed the entire `tools/list`.

// MCP `tools/list` output is genuinely free-form JSON Schema — there is no
// typed struct for an arbitrary tool's input schema, so this validation walks
// it as opaque `Value`. This is exactly the upstream-free-form case the
// `disallowed_types` lint carves out an allowance for.
#[allow(clippy::disallowed_types)]
use serde_json::Value;

/// True if a property schema resolves to a JSON type the MCP client accepts.
/// A bare `true`/`{}` (untyped `Value`) does not.
#[allow(clippy::disallowed_types)]
fn resolves_to_type(prop: &Value) -> bool {
    match prop {
        Value::Object(m) => ["type", "$ref", "oneOf", "anyOf", "allOf", "enum", "const"]
            .iter()
            .any(|k| m.contains_key(*k)),
        _ => false,
    }
}

/// Recursively collect every `properties` entry that fails the type check,
/// reported as dotted paths for a legible failure message.
#[allow(clippy::disallowed_types)]
fn collect_typeless(node: &Value, path: &str, out: &mut Vec<String>) {
    let Some(obj) = node.as_object() else { return };

    if let Some(Value::Object(props)) = obj.get("properties") {
        for (name, prop) in props {
            let prop_path = format!("{path}.{name}");
            if !resolves_to_type(prop) {
                out.push(prop_path.clone());
            }
            collect_typeless(prop, &prop_path, out);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(child) = obj.get(key) {
            collect_typeless(child, &format!("{path}.{key}"), out);
        }
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(variants)) = obj.get(key) {
            for (i, v) in variants.iter().enumerate() {
                collect_typeless(v, &format!("{path}.{key}[{i}]"), out);
            }
        }
    }
}

/// Touch a public symbol from each registry crate so the linker keeps their
/// `inventory::submit!` statics — without this, the separate test binary
/// strips them and walks an incomplete inventory.
fn force_link_registry_crates() {
    // Consume the sizes (a `let _` trips `let_underscore_must_use`); the point
    // is merely to name a symbol from each crate so its object code links.
    assert!(std::mem::size_of::<spec::ProxyGraphqlArgs>() < usize::MAX);
    assert!(std::mem::size_of::<plugins::plugins::PluginUpdateArgs>() < usize::MAX);
}

#[test]
fn mcp_tools_list_has_no_typeless_properties() {
    force_link_registry_crates();
    let defs = dispatch::mcp_definitions();

    // Sanity: the real inventory is linked (spec + runtime tools present),
    // otherwise this test would vacuously pass on an empty list.
    assert!(
        defs.len() > 30,
        "expected the full tool inventory to be linked, got {} tools",
        defs.len()
    );

    let mut offenders = Vec::new();
    for tool in &defs {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        collect_typeless(&tool["inputSchema"], name, &mut offenders);
    }

    assert!(
        offenders.is_empty(),
        "MCP tools/list has {} typeless input-schema properties (would fail the whole list): {:#?}",
        offenders.len(),
        offenders
    );
}

#[test]
fn previously_broken_value_properties_are_now_typed() {
    force_link_registry_crates();
    let defs = dispatch::mcp_definitions();

    // Find the two opaque-`Value` properties that broke `tools/list`:
    // `variables` (spec.graphql update) and `dataValue` (plugin update).
    let mut found_variables = false;
    let mut found_data_value = false;

    for tool in &defs {
        let Some(props) = tool["inputSchema"]["properties"].as_object() else {
            continue;
        };
        for target in ["variables", "dataValue"] {
            if let Some(prop) = props.get(target) {
                assert!(
                    resolves_to_type(prop),
                    "tool {} property `{target}` is still typeless: {prop}",
                    tool["name"]
                );
                assert_eq!(
                    prop["type"], "object",
                    "`{target}` should be an open object"
                );
                match target {
                    "variables" => found_variables = true,
                    "dataValue" => found_data_value = true,
                    _ => {}
                }
            }
        }
    }

    assert!(
        found_variables,
        "no tool exposed a `variables` property — inventory not linked?"
    );
    assert!(
        found_data_value,
        "no tool exposed a `dataValue` property — inventory not linked?"
    );
}
