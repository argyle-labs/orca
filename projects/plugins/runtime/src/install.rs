// Plugin manifest install/remove helpers shared by the `MgmtService` impl, the
// `/api/plugins` REST handler, and tests. Manifest parsing itself lives in
// `db::plugin_manifest` so dial-time consumers (mcp client, plugin_creds sync)
// share one parser.
#![allow(clippy::disallowed_types)]
use anyhow::Result;
use db::{self as db, plugin_manifest, plugins::PluginRow};
use files::ops::expand_tilde;
use std::path::Path;

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
    let (m, abs_path) = plugin_manifest::parse_path(manifest_path)?;
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
        context_injection: m
            .plugin
            .context_injection
            .clone()
            .unwrap_or_else(|| "minimal".into()),
        enabled: true,
        command_map: m.plugin.commands.clone(),
        nav_links: m.plugin.nav_links.clone(),
        search_tools: m.plugin.search_tools.clone(),
        specs_dir,
    };
    db::plugins::upsert(conn, &row)?;

    let display_id = if instance_id != m.plugin.id {
        format!("{} (as '{instance_id}')", m.plugin.id)
    } else {
        instance_id.clone()
    };
    println!(
        "registered plugin {} v{} ({}) from {}",
        display_id, m.plugin.version, m.plugin.tier, abs_path
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
        let dep_id = install_manifest(conn, &dep_path, Some(&dep_instance_id))?;
        db::plugins::add_dep(conn, &instance_id, &dep_id)?;
    }

    Ok(instance_id)
}

/// Parse a manifest just to read the plugin id, without full validation.
fn peek_plugin_id(path: &str) -> Result<String> {
    let (m, _) = plugin_manifest::parse_path(path)?;
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

    fn open_test_db(dir: &std::path::Path) -> rusqlite::Connection {
        db::open_unencrypted(&dir.join("test.db")).unwrap()
    }

    #[test]
    fn install_manifest_writes_row_with_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let path = write_manifest(dir.path(), MINIMAL_MANIFEST);
        let id = install_manifest(&conn, &path, None).unwrap();
        assert_eq!(id, "test-plugin");
        let row = db::plugins::get(&conn, "test-plugin").unwrap().unwrap();
        assert_eq!(row.tier, "personal");
        assert_eq!(row.context_injection, "minimal");
        assert!(row.enabled);
    }

    #[test]
    fn install_manifest_persists_metadata_and_id_override() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_test_db(dir.path());
        let path = write_manifest(dir.path(), FULL_MANIFEST);
        let id = install_manifest(&conn, &path, Some("my-plugin@workspace-a")).unwrap();
        assert_eq!(id, "my-plugin@workspace-a");
        let row = db::plugins::get(&conn, "my-plugin@workspace-a")
            .unwrap()
            .unwrap();
        assert_eq!(row.tier, "team");
        assert_eq!(row.context_injection, "full");
        assert_eq!(row.nav_links.len(), 1);

        // Transport stays in the manifest, not the row — re-parse to verify.
        let (m, _) = plugin_manifest::parse_path(&row.manifest_path).unwrap();
        let mcp = m.plugin.mcp.unwrap();
        assert_eq!(mcp.command, "node");
        assert_eq!(mcp.args, vec!["server.js", "--port", "3000"]);
        assert_eq!(mcp.env.get("LOG_LEVEL").map(|s| s.as_str()), Some("info"));
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

[[plugin.uses]]
path = "dep/orca-plugin.toml"
"#;
        let parent_path = write_manifest(dir.path(), parent_content);
        install_manifest(&conn, &parent_path, None).unwrap();

        assert!(db::plugins::get(&conn, "parent").unwrap().is_some());
        assert!(db::plugins::get(&conn, "child@parent").unwrap().is_some());
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
        install_manifest(&conn, &parent_path, None).unwrap();
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
        install_manifest(&conn, &path, None).unwrap();
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
        let (m, _) = plugin_manifest::parse_path(&path).unwrap();
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
