//! Generic shared-state replication registry.
//!
//! A row type opts into mesh replication with `#[derive(Replicated)]`
//! (see `orca-macro`):
//!
//! ```ignore
//! #[derive(Serialize, Deserialize, Replicated)]
//! #[replicate(table = "users", lww = "updated_at")]
//! pub struct ReplicaUser { pub id: String, /* … */ pub updated_at: String }
//! ```
//!
//! The derive emits one [`ReplicatedRegistration`] into the inventory slice
//! this crate collects, wiring two type-erased fns:
//!   - **export** — `SELECT` every row of `table`, as a JSON array;
//!   - **merge** — upsert each incoming row last-write-wins on the `lww` column,
//!     keyed by the primary key (`pk`, default `id`).
//!
//! The pod mesh engine walks [`registrations`] to build ONE signed bundle
//! (`{ entity_name -> rows }`) per peer rather than a bespoke method per
//! entity. `users` is the first registrant; configs/settings follow.
//!
//! This crate is deliberately tiny and DB-flavoured (it speaks
//! `rusqlite::Connection`) but transport-agnostic — signing, the mTLS dial,
//! and the periodic schedule all live in the pod crate.

// This crate is a registry of *heterogeneous* entity rows — each entity has a
// different typed row, so the common bundle boundary is genuinely free-form
// JSON. The concrete typing happens inside each entity's generated export/merge.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::Result;
use macro_runtime::ReplicatedRegistration;
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::broadcast;

/// Every registered entity, in stable name order.
pub fn registrations() -> Vec<&'static ReplicatedRegistration> {
    let mut v: Vec<_> = inventory::iter::<ReplicatedRegistration>().collect();
    v.sort_by_key(|r| r.name);
    v
}

/// Export every registered entity into a `{ name -> rows }` bundle.
pub fn export_all(conn: &Connection) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for reg in registrations() {
        out.insert(reg.name.to_string(), (reg.export)(conn)?);
    }
    Ok(out)
}

/// Merge an incoming bundle, dispatching each entity to its registered `merge`.
/// Unknown entity names are skipped (forward-compat with peers that replicate
/// entities this host doesn't know). Returns total rows created/updated.
///
/// Merges do NOT emit [`notify_write`] — only origin writes do. Otherwise
/// every push from peer A→B would cascade back as B→A,C,D,…
pub fn merge_bundle(conn: &Connection, bundle: BTreeMap<String, Value>) -> Result<usize> {
    let mut total = 0;
    for reg in registrations() {
        if let Some(rows) = bundle.get(reg.name) {
            match (reg.merge)(conn, rows.clone()) {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("[replicate] merge of '{}' failed: {e:#}", reg.name),
            }
        }
    }
    Ok(total)
}

// ── Write-notify channel — feeds push-on-write fanout in the pod crate ──
//
// Every origin write (insert/update/delete) on a `#[derive(Replicated)]`
// entity calls [`notify_write`]. The pod crate subscribes via [`subscribe`]
// and pushes a freshly-built bundle to all paired peers immediately. The
// 60s pull tick is the backstop, not the primary path.
//
// Replicated merges do NOT notify (see [`merge_bundle`]) — otherwise pushes
// would echo back and amplify.

const WRITE_NOTIFY_CAPACITY: usize = 256;

fn write_notify_sender() -> &'static broadcast::Sender<&'static str> {
    static SENDER: OnceLock<broadcast::Sender<&'static str>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(WRITE_NOTIFY_CAPACITY);
        tx
    })
}

/// Signal that a row was written/updated/deleted on the named replicated
/// entity (e.g. `"users"`). Called by origin write helpers only — never
/// from merge paths. Cheap no-op when no one's subscribed.
pub fn notify_write(entity: &'static str) {
    drop(write_notify_sender().send(entity));
}

/// Subscribe to origin write notifications. Returns a broadcast receiver
/// that yields the entity name of each origin write. Used by pod's
/// push-on-write task.
pub fn subscribe() -> broadcast::Receiver<&'static str> {
    write_notify_sender().subscribe()
}

// ── Merkle-style content roots — cheap divergence check before fetching bundles ──
//
// Each tick, peers exchange these per-entity roots; matching roots → skip the
// full bundle fetch. Hash inputs are canonical (rows from `export` are JSON
// arrays already sorted by pk in the derive's `SELECT … ORDER BY`), so two
// peers with identical row sets always produce the same root.

use sha2::{Digest, Sha256};

/// Per-entity content hash of this host's view. Keyed by entity name.
pub fn roots(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for reg in registrations() {
        let rows = (reg.export)(conn)?;
        let canonical = serde_json::to_vec(&rows)?;
        let mut hasher = Sha256::new();
        hasher.update(&canonical);
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        out.insert(reg.name.to_string(), hex);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;
    use crate::users;

    // The write-notify channel is process-global, so tests that subscribe and
    // assert against received events must serialize against any test that
    // calls `notify_write` directly or via origin-write helpers. A single
    // tokio Mutex held across the test body suffices.
    fn notify_test_lock() -> &'static tokio::sync::Mutex<()> {
        static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[test]
    fn roots_are_deterministic_for_identical_state() {
        let a = test_conn();
        let b = test_conn();
        users::insert(&a, "u1", "scott", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        users::insert(&b, "u1", "scott", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(roots(&a).unwrap(), roots(&b).unwrap());
    }

    /// The divergent-ID case: two hosts each bootstrapped an admin with the
    /// same username but a different local `id`. Merging the peer's row used
    /// to trip `UNIQUE(username_lower)` on a plain INSERT (the users-merge
    /// flood). With the `unique` natural key, the collision resolves as an
    /// LWW UPDATE of the existing local row: merge succeeds, the local `id`
    /// is preserved (FK references stay intact), and there is still ONE row.
    #[test]
    fn merge_resolves_divergent_id_same_username_via_natural_key() {
        let a = test_conn();
        // Local host has scott under id "u1".
        users::insert(
            &a,
            "u1",
            "scott",
            "old-hash",
            "admin",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        // Peer sends scott under a DIFFERENT id "u2", NEWER, different hash.
        let bundle: BTreeMap<String, Value> = [(
            "users".to_string(),
            serde_json::json!([{
                "id": "u2",
                "username": "scott",
                "username_lower": "scott",
                "password_hash": "new-hash",
                "role": "admin",
                "created_at": "2026-01-01T00:00:00Z",
                "password_updated_at": "2026-02-01T00:00:00Z",
                "updated_at": "2026-02-01T00:00:00Z"
            }]),
        )]
        .into_iter()
        .collect();

        // Must NOT error (no UNIQUE-constraint failure).
        let merged = merge_bundle(&a, bundle).unwrap();
        assert_eq!(merged, 1, "the newer peer row should be merged");

        // Exactly one user row, still keyed by the ORIGINAL local id.
        let count: i64 = a
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "must not create a second row for the same user");
        let (id, hash): (String, String) = a
            .query_row(
                "SELECT id, password_hash FROM users WHERE username_lower = 'scott'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            id, "u1",
            "local pk must be preserved so FK refs stay intact"
        );
        assert_eq!(hash, "new-hash", "LWW: newer peer fields win");
    }

    /// LWW must not regress: a peer row OLDER than the local row is skipped
    /// even when it collides on the natural key.
    #[test]
    fn merge_skips_older_peer_row_on_natural_key_collision() {
        let a = test_conn();
        users::insert(
            &a,
            "u1",
            "scott",
            "keep-hash",
            "admin",
            "2026-03-01T00:00:00Z",
        )
        .unwrap();
        let bundle: BTreeMap<String, Value> = [(
            "users".to_string(),
            serde_json::json!([{
                "id": "u2",
                "username": "scott",
                "username_lower": "scott",
                "password_hash": "stale-hash",
                "role": "admin",
                "created_at": "2026-01-01T00:00:00Z",
                "password_updated_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }]),
        )]
        .into_iter()
        .collect();
        let merged = merge_bundle(&a, bundle).unwrap();
        assert_eq!(merged, 0, "older peer row must be skipped");
        let hash: String = a
            .query_row("SELECT password_hash FROM users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            hash, "keep-hash",
            "fresher local data must not be regressed"
        );
    }

    #[test]
    fn roots_change_when_rows_differ() {
        let a = test_conn();
        let b = test_conn();
        users::insert(&a, "u1", "scott", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        // b is empty -> different root
        assert_ne!(
            roots(&a).unwrap().get("users"),
            roots(&b).unwrap().get("users")
        );
    }

    #[test]
    fn roots_cover_every_registered_entity() {
        let conn = test_conn();
        let r = roots(&conn).unwrap();
        for reg in registrations() {
            assert!(
                r.contains_key(reg.name),
                "roots missing registered entity '{}'",
                reg.name
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notify_write_delivers_to_subscriber() {
        let _g = notify_test_lock().lock().await;
        let mut rx = subscribe();
        while rx.try_recv().is_ok() {}
        notify_write("users");
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv error");
        assert_eq!(got, "users");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_insert_fires_notification() {
        let _g = notify_test_lock().lock().await;
        let mut rx = subscribe();
        while rx.try_recv().is_ok() {}
        let conn = test_conn();
        users::insert(&conn, "u1", "alice", "h", "member", "t0").unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("origin write must notify");
        assert_eq!(got.unwrap(), "users");
    }

    // The "merge_bundle must not emit notify_write" invariant cannot be
    // tested at the broadcast layer because the channel is process-global
    // — parallel tests across the crate (and any test that inserts users)
    // leak `"users"` events into any subscriber that exists at the time.
    // The invariant is enforced structurally: see [`merge_bundle`] — it
    // never calls [`notify_write`]. The two tests above
    // (`notify_write_delivers_to_subscriber`, `user_insert_fires_notification`)
    // cover the positive path; for the negative path we rely on the body
    // of `merge_bundle` being trivially small and reviewable.
}
