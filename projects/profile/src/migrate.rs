//! One-shot migrations that seed a profile from baked-in baselines.
//!
//! Currently: relocate the personal-classified agents (badger, meerkat-*)
//! from the embedded baseline into the user's default profile.
//! Once this runs once per install, the agents live on disk under the
//! profile's `agents/` dir and the embedded copies become removable from
//! the public repo.
//!
//! See `project_agent_classification.md` and `project_open_source_split.md`.

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::{Profile, ProfileManager};

/// Agents classified as personal — shipped embedded today, owned by the
/// user's profile going forward. Listed in `project_agent_classification.md`.
pub const PERSONAL_AGENTS: &[&str] = &[
    "badger",
    "meerkat-backup-validate",
    "meerkat-deploy",
    "meerkat-status",
];

/// Settings key recording that the personal-agents migration has run for the
/// given profile id. Versioned so we can re-migrate (e.g. after adding more
/// personal agents to the list) without clobbering user edits.
fn migration_key(profile_id: &str) -> String {
    format!("profile.{profile_id}.embedded_personal_agents_migrated_v1")
}

/// If not already done for this profile, copy the embedded `PERSONAL_AGENTS`
/// into the profile's `agents/` directory and record completion in settings.
///
/// Skips agents that already exist on disk in the profile (so user edits
/// survive). Skips agents that no longer exist in the embedded baseline (e.g.
/// after they're removed from the public repo).
///
/// Returns the number of agents written this run.
pub fn migrate_personal_agents_to_profile(conn: &Connection, profile: &Profile) -> Result<usize> {
    let key = migration_key(&profile.id);
    if matches!(db::get_setting(conn, &key)?.as_deref(), Some("true")) {
        return Ok(0);
    }

    profile.ensure_dirs().context("ensure profile dirs")?;
    let agents_dir = profile.agents_dir();

    let mut written = 0usize;
    for name in PERSONAL_AGENTS {
        let dest = agents_dir.join(format!("{name}.md"));
        if dest.exists() {
            continue;
        }
        let Some(raw) = orca_agents::embedded_agent_raw(name) else {
            tracing::debug!(
                agent = name,
                "personal-agent migration: no embedded copy (already removed?)"
            );
            continue;
        };
        std::fs::write(&dest, raw).with_context(|| format!("write {}", dest.display()))?;
        tracing::info!(
            profile_id = %profile.id,
            agent = name,
            "migrated personal agent into profile"
        );
        written += 1;
    }

    db::set_setting(conn, &key, "true")?;
    Ok(written)
}

impl ProfileManager {
    /// Convenience: run the personal-agents migration against a profile.
    pub fn migrate_personal_agents(&self, conn: &Connection, profile: &Profile) -> Result<usize> {
        migrate_personal_agents_to_profile(conn, profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Role;

    fn test_setup() -> (
        Connection,
        ProfileManager,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let db_tmp = tempfile::TempDir::new().unwrap();
        let conn = db::open_unencrypted(&db_tmp.path().join("test.db")).expect("open_unencrypted");
        let profiles_tmp = tempfile::TempDir::new().unwrap();
        let mgr = ProfileManager::new(profiles_tmp.path().join("profiles"));
        (conn, mgr, db_tmp, profiles_tmp)
    }

    #[test]
    fn migrate_writes_personal_agents_once() {
        let (conn, mgr, _db_td, _p_td) = test_setup();
        let p = mgr.create(&conn, "alice", "default", None).unwrap();

        let n = migrate_personal_agents_to_profile(&conn, &p).unwrap();
        // Wrote however many embedded personal agents currently exist.
        assert!(
            n <= PERSONAL_AGENTS.len(),
            "expected at most {} agents written, got {}",
            PERSONAL_AGENTS.len(),
            n
        );
        // Each successfully-migrated agent now exists on disk.
        for name in PERSONAL_AGENTS {
            let path = p.agents_dir().join(format!("{name}.md"));
            if orca_agents::embedded_agent_raw(name).is_some() {
                assert!(path.exists(), "expected {name}.md on disk after migration");
            }
        }

        // Second run is a no-op.
        let n2 = migrate_personal_agents_to_profile(&conn, &p).unwrap();
        assert_eq!(n2, 0, "second run must be idempotent");
    }

    #[test]
    fn migrate_skips_existing_files() {
        let (conn, mgr, _db_td, _p_td) = test_setup();
        let p = mgr.create(&conn, "alice", "default", None).unwrap();
        p.ensure_dirs().unwrap();

        // Pre-create one of the agent files with custom content.
        let target = PERSONAL_AGENTS[0];
        let user_content = "# my custom edit\nhello world";
        std::fs::write(p.agents_dir().join(format!("{target}.md")), user_content).unwrap();

        migrate_personal_agents_to_profile(&conn, &p).unwrap();
        let on_disk = std::fs::read_to_string(p.agents_dir().join(format!("{target}.md"))).unwrap();
        assert_eq!(
            on_disk, user_content,
            "user-edited agent must not be overwritten"
        );
    }

    #[test]
    fn migrate_is_per_profile() {
        let (conn, mgr, _db_td, _p_td) = test_setup();
        let alice = mgr.create(&conn, "alice", "default", None).unwrap();
        let bob = mgr.create(&conn, "bob", "default", None).unwrap();

        let na = migrate_personal_agents_to_profile(&conn, &alice).unwrap();
        let nb = migrate_personal_agents_to_profile(&conn, &bob).unwrap();
        assert_eq!(na, nb, "both profiles get the same baseline");
    }

    // Silence unused-import warnings in the test module.
    fn _use_role() -> Role {
        Role::Viewer
    }
}
