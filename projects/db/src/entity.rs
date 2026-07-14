//! Canonical-entity registry — the one place that answers "what is the stable
//! identity of this real thing?".
//!
//! Every real thing (host, container, VM, LXC, cluster, service, …) has ONE
//! canonical id: a uuidv7 we mint and own ([`utils::id::new`]). Provider/native
//! ids — a docker container id, a proxmox vmid, a host machine_id, a MAC, a
//! dockge stack ref — are NOT the identity. They are **references** that resolve
//! TO a canonical entity. Keeping identity (the uuid) separate from references
//! (the native ids) is what lets:
//!   - the SAME real thing seen via multiple providers (docker + dockge + unraid
//!     all seeing one container) collapse onto one canonical id, carrying every
//!     reference forward, and
//!   - a canonical id survive a native-id change (a recreated container gets a
//!     new docker id but is the same real thing — the uuid persists, the
//!     docker-id reference updates).
//!
//! This is the reusable core seam of the canonical-identity directive; pod-peer
//! convergence is the host-kind instance of the same merge pattern.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use utils::time::now_secs_since_epoch as now_secs;

/// A reference a provider knows an entity by: a `(kind, value)` pair, globally
/// unique — one reference resolves to exactly one canonical entity. `source` is
/// the provider/discovery path that contributed it (a controller path we keep,
/// never drop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub kind: String,
    pub value: String,
    pub source: Option<String>,
}

impl EntityRef {
    pub fn new(kind: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            value: value.into(),
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub label: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Resolve a set of references to ONE canonical entity id, minting or merging as
/// needed. This is the convergence heart of the registry:
///
///  - **No reference matches** an existing entity → mint a fresh uuidv7, insert
///    the entity and all its references, return the new id.
///  - **References match exactly one** existing entity → that entity is
///    canonical; fold in any new references (`INSERT OR IGNORE`), refresh
///    `label`/`updated_at`, return its id.
///  - **References match several** existing entities → they are the same real
///    thing discovered under divergent references (a re-key, or two providers
///    that each minted before they were correlated). Converge: keep the oldest
///    (lexically-smallest uuidv7 = earliest-minted) as canonical, repoint every
///    other entity's references onto it, delete the losers, then fold in the new
///    references. No reference or controller path is ever dropped.
///
/// `kind`/`label` describe the entity for a fresh mint or a label refresh.
pub fn resolve_entity(
    conn: &Connection,
    kind: &str,
    label: Option<&str>,
    refs: &[EntityRef],
) -> Result<String> {
    let now = now_secs();
    let tx = conn.unchecked_transaction()?;

    // Which existing entities do these references already point at?
    let mut matched: Vec<String> = Vec::new();
    for r in refs {
        if let Some(eid) = tx
            .query_row(
                "SELECT entity_id FROM entity_refs WHERE ref_kind = ?1 AND ref_value = ?2",
                params![r.kind, r.value],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            && !matched.contains(&eid)
        {
            matched.push(eid);
        }
    }

    let canonical = match matched.as_slice() {
        [] => {
            // Fresh thing — mint a canonical uuid.
            let id = utils::id::new();
            tx.execute(
                "INSERT INTO entities (id, kind, label, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id, kind, label, now],
            )?;
            id
        }
        [one] => {
            let id = one.clone();
            if let Some(l) = label {
                tx.execute(
                    "UPDATE entities SET label = ?1, updated_at = ?2 WHERE id = ?3",
                    params![l, now, id],
                )?;
            } else {
                tx.execute(
                    "UPDATE entities SET updated_at = ?1 WHERE id = ?2",
                    params![now, id],
                )?;
            }
            id
        }
        many => {
            // Several entities are really one thing — converge onto the oldest
            // (smallest uuidv7 sorts earliest by mint time).
            let canonical = many.iter().min().expect("non-empty").clone();
            for stale in many {
                if stale == &canonical {
                    continue;
                }
                // Repoint the loser's references onto the canonical entity, then
                // delete it (cascade clears any leftover rows).
                tx.execute(
                    "UPDATE OR IGNORE entity_refs SET entity_id = ?1 WHERE entity_id = ?2",
                    params![canonical, stale],
                )?;
                tx.execute("DELETE FROM entities WHERE id = ?1", params![stale])?;
            }
            if let Some(l) = label {
                tx.execute(
                    "UPDATE entities SET label = ?1, updated_at = ?2 WHERE id = ?3",
                    params![l, now, canonical],
                )?;
            }
            canonical
        }
    };

    // Fold in every reference — additive, never dropping a controller path.
    for r in refs {
        tx.execute(
            "INSERT INTO entity_refs (ref_kind, ref_value, entity_id, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(ref_kind, ref_value) DO UPDATE SET
                 entity_id = excluded.entity_id,
                 source    = COALESCE(excluded.source, entity_refs.source)",
            params![r.kind, r.value, canonical, r.source, now],
        )?;
    }

    tx.commit()?;
    Ok(canonical)
}

/// Fetch an entity by canonical id.
pub fn get_entity(conn: &Connection, id: &str) -> Result<Option<Entity>> {
    Ok(conn
        .query_row(
            "SELECT id, kind, label, created_at, updated_at FROM entities WHERE id = ?1",
            params![id],
            |r| {
                Ok(Entity {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    label: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Every reference recorded for an entity — its full set of provider/native ids
/// and controller paths.
pub fn entity_refs(conn: &Connection, id: &str) -> Result<Vec<EntityRef>> {
    let mut stmt = conn.prepare(
        "SELECT ref_kind, ref_value, source FROM entity_refs
         WHERE entity_id = ?1 ORDER BY ref_kind, ref_value",
    )?;
    let rows = stmt.query_map(params![id], |r| {
        Ok(EntityRef {
            kind: r.get(0)?,
            value: r.get(1)?,
            source: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_conn() -> (TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = db_open(&dir.path().join("orca.db"));
        (dir, conn)
    }

    fn db_open(path: &std::path::Path) -> Connection {
        crate::open_unencrypted(path).expect("open_unencrypted")
    }

    #[test]
    fn mints_fresh_uuid_when_no_ref_matches() {
        let (_d, c) = test_conn();
        let id = resolve_entity(
            &c,
            "container",
            Some("caddy"),
            &[EntityRef::new("docker_id", "abc")],
        )
        .unwrap();
        assert!(utils::id::is_valid(&id));
        let e = get_entity(&c, &id).unwrap().unwrap();
        assert_eq!(e.kind, "container");
        assert_eq!(e.label.as_deref(), Some("caddy"));
    }

    #[test]
    fn same_ref_resolves_to_same_entity() {
        let (_d, c) = test_conn();
        let a = resolve_entity(
            &c,
            "container",
            Some("caddy"),
            &[EntityRef::new("docker_id", "abc")],
        )
        .unwrap();
        let b = resolve_entity(
            &c,
            "container",
            Some("caddy-renamed"),
            &[EntityRef::new("docker_id", "abc")],
        )
        .unwrap();
        assert_eq!(a, b, "same reference → same canonical id");
        // Label refreshed on the existing entity.
        assert_eq!(
            get_entity(&c, &a).unwrap().unwrap().label.as_deref(),
            Some("caddy-renamed")
        );
    }

    #[test]
    fn new_reference_folds_onto_existing_entity() {
        let (_d, c) = test_conn();
        // Discovered first via docker.
        let a =
            resolve_entity(&c, "container", None, &[EntityRef::new("docker_id", "abc")]).unwrap();
        // Later the SAME thing is seen with an added reference (e.g. a dockge
        // stack ref) alongside the docker id.
        let b = resolve_entity(
            &c,
            "container",
            None,
            &[
                EntityRef::new("docker_id", "abc"),
                EntityRef::new("dockge_stack", "media/caddy"),
            ],
        )
        .unwrap();
        assert_eq!(a, b);
        let refs = entity_refs(&c, &a).unwrap();
        assert_eq!(refs.len(), 2, "both references live on the one entity");
    }

    #[test]
    fn converges_multiple_entities_when_a_ref_correlates_them() {
        let (_d, c) = test_conn();
        // Two providers each minted a separate entity for what is really one
        // container (docker saw the docker id; dockge saw a stack ref).
        let via_docker =
            resolve_entity(&c, "container", None, &[EntityRef::new("docker_id", "abc")]).unwrap();
        let via_dockge = resolve_entity(
            &c,
            "container",
            None,
            &[EntityRef::new("dockge_stack", "media/caddy")],
        )
        .unwrap();
        assert_ne!(via_docker, via_dockge);
        // A later discovery carries BOTH references → converge to one.
        let merged = resolve_entity(
            &c,
            "container",
            None,
            &[
                EntityRef::new("docker_id", "abc"),
                EntityRef::new("dockge_stack", "media/caddy"),
            ],
        )
        .unwrap();
        // Canonical = the oldest (earliest-minted uuidv7).
        let expected = std::cmp::min(via_docker.clone(), via_dockge.clone());
        assert_eq!(merged, expected);
        // The loser is gone; all references now hang off the survivor.
        let survivors: i64 = c
            .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(survivors, 1, "two entities converged into one");
        assert_eq!(entity_refs(&c, &merged).unwrap().len(), 2);
    }

    #[test]
    fn recreated_native_id_keeps_canonical_via_stable_ref() {
        let (_d, c) = test_conn();
        // A container carries a stable app-level ref (e.g. compose service) plus
        // its ephemeral docker id.
        let a = resolve_entity(
            &c,
            "container",
            None,
            &[
                EntityRef::new("dockge_stack", "media/caddy"),
                EntityRef::new("docker_id", "old"),
            ],
        )
        .unwrap();
        // Recreated: same stable ref, NEW docker id.
        let b = resolve_entity(
            &c,
            "container",
            None,
            &[
                EntityRef::new("dockge_stack", "media/caddy"),
                EntityRef::new("docker_id", "new"),
            ],
        )
        .unwrap();
        assert_eq!(a, b, "canonical id survives a native-id change");
    }
}
