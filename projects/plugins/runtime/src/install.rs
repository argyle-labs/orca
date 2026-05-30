// Plugin manifest parsing + install/remove helpers shared by the
// `MgmtService` impl, the `/api/plugins` REST handler, and tests.
#![allow(clippy::disallowed_types)]
use anyhow::{Context, Result};
use db::{self as db, plugins::PluginRow};
use files::ops::expand_tilde;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

// ── Manifest parsing ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ManifestMcp {
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Env var name whose value is the Bearer token for HTTP/SSE transport.
    token_env: Option<String>,
    /// HTTP/SSE endpoints tried in priority order (public domain → LAN → tailscale).
    /// Single string `url` is a shorthand for a one-element list.
    url: Option<String>,
    #[serde(default)]
    urls: Vec<String>,
}

#[derive(Deserialize, Default)]
struct ManifestSpecs {
    /// Filesystem path (supports ~/) where this plugin's spec files live.
    dir: Option<String>,
}

#[derive(Deserialize, Default)]
struct ManifestUses {
    /// Path to the dependency's orca-plugin.toml (relative to this manifest or absolute/~/…).
    path: String,
    /// Override the instance id for this dependency. Allows the same plugin template
    /// to be used multiple times with different credentials (e.g. atlassian@rebuy vs atlassian@infra).
    /// Defaults to "{dep_plugin_id}@{parent_id}" when not specified.
    id: Option<String>,
}

#[derive(Deserialize)]
struct ManifestPlugin {
    id: String,
    version: String,
    tier: String,
    #[serde(default)]
    context_injection: Option<String>,
    #[serde(default)]
    mcp: Option<ManifestMcp>,
    /// Maps universal command name → plugin's internal MCP tool name.
    #[serde(default)]
    commands: HashMap<String, String>,
    /// Sidebar nav links this plugin contributes: [{href, label, section?}]
    #[serde(default)]
    nav_links: Vec<serde_json::Value>,
    /// MCP tools this plugin exposes for orca's unified search (Cmd+K).
    #[serde(default)]
    search_tools: Vec<db::PluginSearchTool>,
    /// Optional directory containing spec files served with this plugin's namespace.
    #[serde(default)]
    specs: Option<ManifestSpecs>,
    /// Other plugins this plugin extends. Dependencies are installed automatically.
    #[serde(default, rename = "uses")]
    uses: Vec<ManifestUses>,
}

#[derive(Deserialize)]
struct Manifest {
    plugin: ManifestPlugin,
}

fn parse_manifest(path: &str) -> Result<(Manifest, String)> {
    let resolved = expand_tilde(path);

    let abs = std::fs::canonicalize(&resolved)
        .with_context(|| format!("manifest not found: {resolved}"))?;

    let text = std::fs::read_to_string(&abs)
        .with_context(|| format!("failed to read {}", abs.display()))?;

    let manifest: Manifest = toml::from_str(&text)
        .with_context(|| format!("invalid orca-plugin.toml at {}", abs.display()))?;

    Ok((manifest, abs.to_string_lossy().into_owned()))
}

/// Public entry point: install a plugin from a manifest path.
/// `instance_id` overrides the plugin's own id (for multi-instance scenarios).
pub fn install_plugin(manifest_path: &str, instance_id: Option<&str>) -> Result<String> {
    let conn = db::open_default()?;
    install_manifest(&conn, manifest_path, instance_id)
}

/// Public entry point: remove a plugin and cascade-remove exclusive deps.
pub fn remove_plugin(id: &str) -> Result<bool> {
    let conn = db::open_default()?;
    let deps = db::plugins::list_deps(&conn, id)?;
    db::plugins::remove_deps(&conn, id)?;
    for dep_id in &deps {
        if !db::plugins::has_parent(&conn, dep_id)? {
            db::plugins::remove(&conn, dep_id)?;
        }
    }
    db::plugins::remove(&conn, id)
}

/// Install a single plugin manifest into the DB.
///
/// - `instance_id_override`: use this id instead of the one declared in the toml.
///   Enables multiple instances of the same plugin template (e.g. `atlassian@infra-a`
///   and `atlassian@infra-b`) each with their own credentials and MCP connection.
///
/// Returns the instance id that was registered.
fn install_manifest(
    conn: &rusqlite::Connection,
    manifest_path: &str,
    instance_id_override: Option<&str>,
) -> Result<String> {
    let (m, abs_path) = parse_manifest(manifest_path)?;
    let instance_id = instance_id_override.unwrap_or(&m.plugin.id).to_string();
    let specs_dir = m
        .plugin
        .specs
        .as_ref()
        .and_then(|s| s.dir.as_deref())
        .map(expand_tilde);
    let row = PluginRow {
        id: instance_id.clone(),
        manifest_path: abs_path.clone(),
        tier: m.plugin.tier.clone(),
        mcp_command: m
            .plugin
            .mcp
            .as_ref()
            .map(|mcp| mcp.command.clone())
            .filter(|c| !c.is_empty()),
        mcp_args: m
            .plugin
            .mcp
            .as_ref()
            .map(|mcp| mcp.args.clone())
            .unwrap_or_default(),
        mcp_env: m
            .plugin
            .mcp
            .as_ref()
            .map(|mcp| mcp.env.clone())
            .unwrap_or_default(),
        mcp_token_env: m.plugin.mcp.as_ref().and_then(|mcp| mcp.token_env.clone()),
        mcp_urls: m
            .plugin
            .mcp
            .as_ref()
            .map(|mcp| {
                // `urls` list takes precedence; `url` is a single-entry shorthand.
                if !mcp.urls.is_empty() {
                    mcp.urls.clone()
                } else if let Some(u) = &mcp.url {
                    vec![u.clone()]
                } else {
                    vec![]
                }
            })
            .unwrap_or_default(),
        context_injection: m
            .plugin
            .context_injection
            .clone()
            .unwrap_or_else(|| "minimal".into()),
        enabled: true,
        command_map: m.plugin.commands.clone(),
        nav_links: m.plugin.nav_links.clone(),
        search_tools: m.plugin.search_tools,
        specs_dir,
    };
    db::plugins::upsert(conn, &row)?;

    let display_id = if instance_id != m.plugin.id {
        format!("{} (as '{instance_id}')", m.plugin.id)
    } else {
        instance_id.clone()
    };
    println!(
        "registered plugin {} v{} ({}) [mode: {}] from {}",
        display_id, m.plugin.version, m.plugin.tier, mode, abs_path
    );

    // Recursively install uses, resolving paths relative to this manifest's directory.
    let manifest_dir = Path::new(&abs_path).parent().unwrap_or(Path::new("."));
    for dep in &m.plugin.uses {
        let dep_path = if dep.path.starts_with('/') || dep.path.starts_with('~') {
            dep.path.clone()
        } else {
            manifest_dir.join(&dep.path).to_string_lossy().into_owned()
        };
        // Resolve the dep's base id from its manifest to build the default scoped id.
        let dep_base_id = peek_plugin_id(&dep_path).unwrap_or_else(|_| "plugin".to_string());
        let dep_instance_id = dep
            .id
            .clone()
            .unwrap_or_else(|| format!("{dep_base_id}@{instance_id}"));
        let dep_id = install_manifest(conn, &dep_path, Some(&dep_instance_id), Some(&mode))?;
        db::plugins::add_dep(conn, &instance_id, &dep_id)?;
    }

    Ok(instance_id)
}

/// Parse a manifest just to read the plugin id, without full validation.
fn peek_plugin_id(path: &str) -> Result<String> {
    let (m, _) = parse_manifest(path)?;
    Ok(m.plugin.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("orca-plugin.toml");
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    const MINIMAL_MANIFEST: &str = r#"
[plugin]
id = "test-plugin"
version = "1.0.0"
tier = "personal"
"#;

    const FULL_MANIFEST: &str = r#"
[plugin]
id = "my-plugin"
version = "2.3.1"
tier = "team"
mode = "rebuy"
context_injection = "full"

[plugin.mcp]
command = "node"
args = ["server.js", "--port", "3000"]

[plugin.mcp.env]
LOG_LEVEL = "info"

[[plugin.nav_links]]
href = "/dashboard"
label = "Dashboard"
"#;

    // ── parse_manifest ────────────────────────────────────────────────────────

    #[test]
    fn parse_manifest_minimal_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), MINIMAL_MANIFEST);
        let (m, abs) = parse_manifest(&path).unwrap();
        assert_eq!(m.plugin.id, "test-plugin");
        assert_eq!(m.plugin.version, "1.0.0");
        assert_eq!(m.plugin.tier, "personal");
        assert_eq!(m.plugin.mode, "orca", "default mode should be orca");
        assert!(abs.contains("orca-plugin.toml"));
    }

    #[test]
    fn parse_manifest_full_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), FULL_MANIFEST);
        let (m, _) = parse_manifest(&path).unwrap();
        assert_eq!(m.plugin.id, "my-plugin");
        assert_eq!(m.plugin.mode, "rebuy");
        assert_eq!(m.plugin.context_injection, Some("full".into()));
        let mcp = m.plugin.mcp.unwrap();
        assert_eq!(mcp.command, "node");
        assert_eq!(mcp.args, vec!["server.js", "--port", "3000"]);
        assert_eq!(mcp.env.get("LOG_LEVEL").map(|s| s.as_str()), Some("info"));
    }

    #[test]
    fn parse_manifest_errors_on_missing_file() {
        match parse_manifest("/tmp/__no_such_manifest__.toml") {
            Ok(_) => panic!("expected error for missing file"),
            Err(e) => assert!(e.to_string().contains("manifest not found"), "got: {e}"),
        }
    }

    #[test]
    fn parse_manifest_errors_on_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), "this is not valid toml {{{{");
        match parse_manifest(&path) {
            Ok(_) => panic!("expected error for invalid TOML"),
            Err(e) => assert!(
                e.to_string().contains("invalid orca-plugin.toml"),
                "got: {e}"
            ),
        }
    }

    #[test]
    fn parse_manifest_errors_on_missing_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), "[plugin]\nid = \"x\"\n");
        match parse_manifest(&path) {
            Ok(_) => panic!("expected error for missing fields"),
            Err(e) => assert!(
                e.to_string().contains("invalid orca-plugin.toml"),
                "got: {e}"
            ),
        }
    }

    // ── peek_plugin_id ────────────────────────────────────────────────────────

    #[test]
    fn peek_plugin_id_returns_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), MINIMAL_MANIFEST);
        assert_eq!(peek_plugin_id(&path).unwrap(), "test-plugin");
    }

    #[test]
    fn peek_plugin_id_errors_on_missing_file() {
        assert!(peek_plugin_id("/tmp/__no_such_file__.toml").is_err());
    }

    // ── mcp url resolution in install_manifest ────────────────────────────────

    #[test]
    fn manifest_mcp_url_shorthand_becomes_vec() {
        let content = r#"
[plugin]
id = "http-plugin"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
url = "http://localhost:8080"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), content);
        let (m, _) = parse_manifest(&path).unwrap();
        let mcp = m.plugin.mcp.unwrap();
        assert_eq!(mcp.url.as_deref(), Some("http://localhost:8080"));
        assert!(
            mcp.urls.is_empty(),
            "urls list should be empty when only url shorthand is set"
        );
    }

    #[test]
    fn manifest_mcp_urls_list_takes_precedence() {
        let content = r#"
[plugin]
id = "multi-url"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
url = "http://public.example.com"
urls = ["http://lan.local", "http://tailscale.local"]
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), content);
        let (m, _) = parse_manifest(&path).unwrap();
        let mcp = m.plugin.mcp.unwrap();
        assert_eq!(mcp.urls, vec!["http://lan.local", "http://tailscale.local"]);
    }

    #[test]
    fn manifest_default_mode_is_orca() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), MINIMAL_MANIFEST);
        let (m, _) = parse_manifest(&path).unwrap();
        assert_eq!(m.plugin.mode, "orca");
    }

    fn open_test_db(dir: &std::path::Path) -> rusqlite::Connection {
        db::open_unencrypted(&dir.join("test.db")).unwrap()
    }

    #[test]
    fn install_manifest_writes_row_with_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let path = write_manifest(dir.path(), MINIMAL_MANIFEST);
        let id = install_manifest(&conn, &path, None, None).unwrap();
        assert_eq!(id, "test-plugin");
        let row = db::plugins::get(&conn, "test-plugin").unwrap().unwrap();
        assert_eq!(row.tier, "personal");
        assert_eq!(row.mode, "orca");
        assert_eq!(row.context_injection, "minimal");
        assert!(row.mcp_command.is_none());
        assert!(row.mcp_urls.is_empty());
        assert!(row.enabled);
    }

    #[test]
    fn install_manifest_persists_full_mcp_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let path = write_manifest(dir.path(), FULL_MANIFEST);
        let id = install_manifest(&conn, &path, Some("my-plugin@rebuy"), Some("custom")).unwrap();
        assert_eq!(id, "my-plugin@rebuy");
        let row = db::plugins::get(&conn, "my-plugin@rebuy").unwrap().unwrap();
        assert_eq!(row.tier, "team");
        assert_eq!(row.mode, "custom");
        assert_eq!(row.context_injection, "full");
        assert_eq!(row.mcp_command.as_deref(), Some("node"));
        assert_eq!(row.mcp_args, vec!["server.js", "--port", "3000"]);
        assert_eq!(
            row.mcp_env.get("LOG_LEVEL").map(|s| s.as_str()),
            Some("info")
        );
        assert_eq!(row.nav_links.len(), 1);
    }

    #[test]
    fn install_manifest_mcp_url_shorthand_becomes_single_element_urls() {
        let content = r#"
[plugin]
id = "urlp"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
command = ""
url = "http://localhost:8080"
"#;
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let path = write_manifest(dir.path(), content);
        install_manifest(&conn, &path, None, None).unwrap();
        let row = db::plugins::get(&conn, "urlp").unwrap().unwrap();
        assert_eq!(row.mcp_urls, vec!["http://localhost:8080".to_string()]);
        assert!(
            row.mcp_command.is_none(),
            "empty command must be filtered to None"
        );
    }

    #[test]
    fn install_manifest_mcp_urls_list_wins_over_url() {
        let content = r#"
[plugin]
id = "multi"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
command = "x"
url = "http://public"
urls = ["http://lan", "http://ts"]
token_env = "TOK"
"#;
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let path = write_manifest(dir.path(), content);
        install_manifest(&conn, &path, None, None).unwrap();
        let row = db::plugins::get(&conn, "multi").unwrap().unwrap();
        assert_eq!(
            row.mcp_urls,
            vec!["http://lan".to_string(), "http://ts".to_string()]
        );
        assert_eq!(row.mcp_token_env.as_deref(), Some("TOK"));
    }

    #[test]
    fn install_manifest_recursively_installs_uses_with_scoped_ids() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());

        let dep_dir = dir.path().join("dep");
        std::fs::create_dir_all(&dep_dir).unwrap();
        let _dep_path = write_manifest(
            &dep_dir,
            r#"
[plugin]
id = "child"
version = "0.1.0"
tier = "personal"
"#,
        );

        let parent_content = r#"
[plugin]
id = "parent"
version = "1.0.0"
tier = "personal"
mode = "rebuy"

[[plugin.uses]]
path = "dep/orca-plugin.toml"
"#;
        let parent_path = write_manifest(dir.path(), parent_content);
        install_manifest(&conn, &parent_path, None, None).unwrap();

        let parent_row = db::plugins::get(&conn, "parent").unwrap().unwrap();
        assert_eq!(parent_row.mode, "rebuy");
        let dep_row = db::plugins::get(&conn, "child@parent").unwrap().unwrap();
        assert_eq!(dep_row.mode, "rebuy", "dep should inherit parent's mode");
        let deps = db::plugins::list_deps(&conn, "parent").unwrap();
        assert!(deps.contains(&"child@parent".to_string()));
    }

    #[test]
    fn install_manifest_uses_id_override_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());

        let dep_dir = dir.path().join("dep2");
        std::fs::create_dir_all(&dep_dir).unwrap();
        write_manifest(
            &dep_dir,
            r#"
[plugin]
id = "child"
version = "0.1.0"
tier = "personal"
"#,
        );

        let parent_content = r#"
[plugin]
id = "p2"
version = "1.0.0"
tier = "personal"

[[plugin.uses]]
path = "dep2/orca-plugin.toml"
id = "explicit-id"
"#;
        let parent_path = write_manifest(dir.path(), parent_content);
        install_manifest(&conn, &parent_path, None, None).unwrap();
        assert!(db::plugins::get(&conn, "explicit-id").unwrap().is_some());
    }

    #[test]
    fn install_manifest_specs_dir_is_expanded_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let content = r#"
[plugin]
id = "withspecs"
version = "1.0.0"
tier = "personal"

[plugin.specs]
dir = "/tmp/orca-test-specs"
"#;
        let path = write_manifest(dir.path(), content);
        install_manifest(&conn, &path, None, None).unwrap();
        let row = db::plugins::get(&conn, "withspecs").unwrap().unwrap();
        assert_eq!(row.specs_dir.as_deref(), Some("/tmp/orca-test-specs"));
    }

    #[test]
    fn manifest_commands_map_parsed() {
        let content = r#"
[plugin]
id = "cmd-plugin"
version = "1.0.0"
tier = "personal"

[plugin.commands]
search = "mcp_search"
deploy = "mcp_deploy"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), content);
        let (m, _) = parse_manifest(&path).unwrap();
        assert_eq!(
            m.plugin.commands.get("search").map(|s| s.as_str()),
            Some("mcp_search")
        );
        assert_eq!(
            m.plugin.commands.get("deploy").map(|s| s.as_str()),
            Some("mcp_deploy")
        );
    }
}
