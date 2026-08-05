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
use std::sync::{Mutex, OnceLock};

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

/// True iff `name` is a mesh-replicated entity. Endpoint-table names equal
/// their entity name (empty namespace), so core's `exec_db_op` uses this to
/// decide whether a `DbOp` delete/write against a table should record a
/// command-log op ([`crate::replication_ops`]).
pub fn is_registered(name: &str) -> bool {
    inventory::iter::<ReplicatedRegistration>().any(|r| r.name == name)
}

/// Resolve a runtime table name to its `&'static` replicated entity name, if it
/// is registered. Lets the generic plugin/endpoint CRUD path — which only knows
/// the table name as a runtime `String` — reuse [`notify_write`] (invalidate the
/// content-root memo + wake push-on-write) with the correct static entity name.
pub fn registered_entity(name: &str) -> Option<&'static str> {
    inventory::iter::<ReplicatedRegistration>()
        .find(|r| r.name == name)
        .map(|r| r.name)
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
///
/// Ordering matters for command-log delete ([`crate::replication_ops`]): the
/// op-log entity is merged FIRST so pending deletes are known before any domain
/// row lands, and [`crate::replication_ops::apply_pending_deletes`] runs LAST so
/// a stale peer row re-imported by a domain merge in this same tick is removed
/// again — closing the resurrection race.
pub fn merge_bundle(conn: &Connection, bundle: BTreeMap<String, Value>) -> Result<usize> {
    let mut total = 0;

    // Phase 1: merge the op-log first, so the delete/upsert transitions it
    // carries are authoritative before we touch domain tables.
    if let Some(rows) = bundle.get(crate::replication_ops::ENTITY) {
        for reg in registrations() {
            if reg.name == crate::replication_ops::ENTITY {
                match (reg.merge)(conn, rows.clone()) {
                    Ok(n) => total += n,
                    Err(e) => tracing::warn!("[replicate] merge of op-log failed: {e:#}"),
                }
            }
        }
    }

    // Phase 2: merge every domain entity (LWW upsert). This may re-import a row
    // a peer still holds but we have since deleted — phase 3 cleans that up.
    for reg in registrations() {
        if reg.name == crate::replication_ops::ENTITY {
            continue;
        }
        if let Some(rows) = bundle.get(reg.name) {
            match (reg.merge)(conn, rows.clone()) {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("[replicate] merge of '{}' failed: {e:#}", reg.name),
            }
        }
    }

    // Phase 3: enforce deletions — physically remove any domain row the merged
    // op-log marks deleted (including anything phase 2 just resurrected).
    let deleted = match crate::replication_ops::apply_pending_deletes(conn) {
        Ok(n) if n > 0 => {
            tracing::debug!("[replicate] applied {n} pending delete(s)");
            n
        }
        Ok(_) => 0,
        Err(e) => {
            tracing::warn!("[replicate] apply_pending_deletes failed: {e:#}");
            0
        }
    };

    // A merge that changed local rows makes our cached content-roots stale.
    // Clear the whole cache — merges are the cold path (divergence only), so a
    // full recompute on the next `roots()` call is cheap relative to the churn
    // avoided on every in-sync tick.
    if total > 0 || deleted > 0 {
        invalidate_all_roots();
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
    // An origin write changed this entity's rows → its cached content-root is
    // stale. Drop just that entry so the next `roots()` recomputes one table,
    // not all of them.
    invalidate_root(entity);
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

/// In-memory memo of per-entity content roots. Computing a root re-exports and
/// serializes the entire table, so on a steady fleet (no writes) we'd redo that
/// work on every 5s pull tick, for every peer. Instead we cache each entity's
/// root and invalidate only the entities that actually change: `notify_write`
/// drops one entry on every origin write, `merge_bundle` clears all after a
/// merge that landed rows. In steady state `roots()` is a HashMap read.
// Keyed by DB file path → { entity → root }. A daemon has exactly one durable
// `orca.db`, so in production this holds a single path bucket. Keying by path
// keeps multi-DB test processes correct, and in-memory connections (`:memory:`,
// used by unit tests) bypass the cache entirely — always recomputed.
type RootMap = std::collections::HashMap<&'static str, String>;
static ROOTS_CACHE: OnceLock<Mutex<std::collections::HashMap<String, RootMap>>> = OnceLock::new();

fn roots_cache() -> &'static Mutex<std::collections::HashMap<String, RootMap>> {
    ROOTS_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// A file-backed connection's cache key, or `None` for in-memory (bypass).
fn cache_key(conn: &Connection) -> Option<String> {
    match conn.path() {
        Some(p) if !p.is_empty() && p != ":memory:" => Some(p.to_string()),
        _ => None,
    }
}

/// Drop one entity's cached root across all DBs (called on origin writes). In
/// production there's one DB, so this clears exactly one entry.
pub fn invalidate_root(entity: &str) {
    for m in roots_cache().lock().unwrap().values_mut() {
        m.remove(entity);
    }
}

/// Drop the whole roots cache (called after a merge that changed local rows).
pub fn invalidate_all_roots() {
    roots_cache().lock().unwrap().clear();
}

fn compute_root(conn: &Connection, reg: &ReplicatedRegistration) -> Result<String> {
    let rows = (reg.export)(conn)?;
    let canonical = serde_json::to_vec(&rows)?;
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Per-entity content hash of this host's view. Keyed by entity name. Served
/// from the memo where possible; only entities whose root was invalidated
/// (written locally or merged from a peer) are recomputed.
pub fn roots(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let key = cache_key(conn);
    let mut out = BTreeMap::new();
    for reg in registrations() {
        let cached = key.as_ref().and_then(|k| {
            roots_cache()
                .lock()
                .unwrap()
                .get(k)
                .and_then(|m| m.get(reg.name).cloned())
        });
        let hex = match cached {
            Some(h) => h,
            None => {
                let h = compute_root(conn, reg)?;
                if let Some(k) = key.as_ref() {
                    roots_cache()
                        .lock()
                        .unwrap()
                        .entry(k.clone())
                        .or_default()
                        .insert(reg.name, h.clone());
                }
                h
            }
        };
        out.insert(reg.name.to_string(), hex);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;
    use crate::testing::{fx_delete, fx_find, fx_insert};

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
        fx_insert(&a, "u1", "scott", "h", "2026-01-01T00:00:00Z");
        fx_insert(&b, "u1", "scott", "h", "2026-01-01T00:00:00Z");
        assert_eq!(roots(&a).unwrap(), roots(&b).unwrap());
    }

    /// The divergent-ID case: two hosts each bootstrapped the same natural key
    /// under a different local `id`. Merging the peer's row used to trip
    /// `UNIQUE(name_lower)` on a plain INSERT. With the `unique` natural key, the
    /// collision resolves as an LWW UPDATE of the existing local row: merge
    /// succeeds, the local `id` is preserved (FK references stay intact), and
    /// there is still ONE row.
    #[test]
    fn merge_resolves_divergent_id_same_username_via_natural_key() {
        let a = test_conn();
        // Local host has scott under id "u1".
        fx_insert(&a, "u1", "scott", "old-hash", "2026-01-01T00:00:00Z");

        // Peer sends scott under a DIFFERENT id "u2", NEWER, different payload.
        let bundle: BTreeMap<String, Value> = [(
            "replica_fixture".to_string(),
            serde_json::json!([{
                "id": "u2",
                "name": "scott",
                "name_lower": "scott",
                "payload": "new-hash",
                "updated_at": "2026-02-01T00:00:00Z"
            }]),
        )]
        .into_iter()
        .collect();

        // Must NOT error (no UNIQUE-constraint failure).
        let merged = merge_bundle(&a, bundle).unwrap();
        assert_eq!(merged, 1, "the newer peer row should be merged");

        // Exactly one row, still keyed by the ORIGINAL local id.
        let count: i64 = a
            .query_row("SELECT COUNT(*) FROM replica_fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "must not create a second row for the same key");
        let (id, payload) = fx_find(&a, "scott").unwrap();
        assert_eq!(
            id, "u1",
            "local pk must be preserved so FK refs stay intact"
        );
        assert_eq!(payload, "new-hash", "LWW: newer peer fields win");
    }

    /// LWW must not regress: a peer row OLDER than the local row is skipped
    /// even when it collides on the natural key.
    #[test]
    fn merge_skips_older_peer_row_on_natural_key_collision() {
        let a = test_conn();
        fx_insert(&a, "u1", "scott", "keep-hash", "2026-03-01T00:00:00Z");
        let bundle: BTreeMap<String, Value> = [(
            "replica_fixture".to_string(),
            serde_json::json!([{
                "id": "u2",
                "name": "scott",
                "name_lower": "scott",
                "payload": "stale-hash",
                "updated_at": "2026-01-01T00:00:00Z"
            }]),
        )]
        .into_iter()
        .collect();
        let merged = merge_bundle(&a, bundle).unwrap();
        assert_eq!(merged, 0, "older peer row must be skipped");
        let (_, payload) = fx_find(&a, "scott").unwrap();
        assert_eq!(
            payload, "keep-hash",
            "fresher local data must not be regressed"
        );
    }

    /// The rc.26 resurrection race, end to end: host A deletes a row while
    /// host B still holds it. Neither state-gossip direction may bring it back.
    /// This is the guarantee the command-log exists to provide.
    #[test]
    fn delete_propagates_and_never_resurrects_under_mutual_merge() {
        let a = test_conn();
        let b = test_conn();
        fx_insert(&a, "u1", "scott", "h", "2026-01-01T00:00:00Z");
        fx_insert(&b, "u1", "scott", "h", "2026-01-01T00:00:00Z");

        // A deletes scott (hard delete + command-log op).
        assert!(fx_delete(&a, "u1"));
        assert!(fx_find(&a, "scott").is_none());

        // A pulls B's STALE bundle (B still holds scott). Before the op-log this
        // re-inserted the row on A — now apply_pending_deletes removes it again.
        merge_bundle(&a, export_all(&b).unwrap()).unwrap();
        assert!(
            fx_find(&a, "scott").is_none(),
            "stale peer row must not resurrect the deleted row on A"
        );

        // B pulls A's bundle (carries the delete op) → B converges to deleted.
        merge_bundle(&b, export_all(&a).unwrap()).unwrap();
        assert!(
            fx_find(&b, "scott").is_none(),
            "delete op must propagate and remove the row on B"
        );
    }

    /// Delete-then-recreate the same key propagates as a resurrection: the newer
    /// `upsert` op out-votes the delete, so the re-created row survives fleet-wide.
    #[test]
    fn recreate_after_delete_propagates_and_survives() {
        let a = test_conn();
        let b = test_conn();
        fx_insert(&a, "u1", "scott", "h1", "2026-01-01T00:00:00Z");
        merge_bundle(&b, export_all(&a).unwrap()).unwrap();

        // A deletes then re-creates scott with a fresh id + newer payload.
        fx_delete(&a, "u1");
        fx_insert(&a, "u2", "scott", "h2", "2026-02-01T00:00:00Z");

        // B merges A: the re-create must win — scott survives with the new payload
        // (natural-key merge preserves B's local pk; the point is it is NOT deleted).
        merge_bundle(&b, export_all(&a).unwrap()).unwrap();
        let got = fx_find(&b, "scott");
        assert!(
            got.is_some(),
            "re-created row must propagate, not stay deleted"
        );
        assert_eq!(got.unwrap().1, "h2", "newer re-create fields win");
    }

    #[test]
    fn roots_change_when_rows_differ() {
        let a = test_conn();
        let b = test_conn();
        fx_insert(&a, "u1", "scott", "h", "2026-01-01T00:00:00Z");
        // b is empty -> different root
        assert_ne!(
            roots(&a).unwrap().get("replica_fixture"),
            roots(&b).unwrap().get("replica_fixture")
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
        notify_write("replica_fixture");
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv error");
        assert_eq!(got, "replica_fixture");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_insert_fires_notification() {
        let _g = notify_test_lock().lock().await;
        let mut rx = subscribe();
        while rx.try_recv().is_ok() {}
        let conn = test_conn();
        fx_insert(&conn, "u1", "alice", "h", "t0");
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("origin write must notify");
        assert_eq!(got.unwrap(), "replica_fixture");
    }

    // The "merge_bundle must not emit notify_write" invariant cannot be
    // tested at the broadcast layer because the channel is process-global
    // — parallel tests across the crate (and any test that inserts a fixture row)
    // leak fixture events into any subscriber that exists at the time.
    // The invariant is enforced structurally: see [`merge_bundle`] — it
    // never calls [`notify_write`]. The two tests above
    // (`notify_write_delivers_to_subscriber`, `user_insert_fires_notification`)
    // cover the positive path; for the negative path we rely on the body
    // of `merge_bundle` being trivially small and reviewable.
}
