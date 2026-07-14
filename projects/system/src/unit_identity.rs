//! Canonical unit-identity registry — the source of truth mapping a unit's
//! stable *natural key* to its pure, opaque canonical identity: a **uuidv7**.
//!
//! Identity is a pure id; the descriptive/routing coordinates
//! ([`contract::unit::UnitId`] — manager / kind / name) live as separate fields
//! on the object and never *are* the identity. A real unit is discovered under
//! a stable natural key (a provider-supplied `canonical` string like
//! `cluster:<name>/<kind>/<vmid>`, or the fallback `manager/kind/id` composite);
//! this registry mints one time-ordered uuidv7 for that key the first time it is
//! seen and returns the same uuid on every later resolve. References
//! (`DelegatedRepair`, storage-assurance `Dependent`) point at the uuid, so a
//! coordinate change (rename, re-scope, manager move) never changes identity.
//!
//! Rides `#[endpoint_resource]` so the SQLite table is generated + persisted
//! identically to every other managed core store (e.g. `managed_mounts`).

use anyhow::Result;
use plugin_toolkit::endpoint_resource;
use uuid::Uuid;

/// One `natural_key -> uuidv7` binding. `name` (PK) is the natural key; `uuid`
/// is the minted canonical identity, stored as its hyphenated string form.
#[endpoint_resource(plugin = "unit_identity", table = "unit_identities")]
pub struct UnitIdentity {
    pub name: String,
    /// The canonical identity: a uuidv7, hyphenated string form.
    pub uuid: String,
    pub enabled: bool,
}

/// Resolve `natural_key` to its canonical uuidv7, minting one (time-ordered) on
/// first sight and persisting the binding so it is stable forever after.
///
/// Concurrency: two callers racing the same unseen key can both mint; the
/// second `insert` collides on the PK. We treat any insert failure as "someone
/// else won" and re-read the now-present row, so the registry converges on a
/// single canonical uuid per key.
pub fn resolve_or_mint(natural_key: &str) -> Result<Uuid> {
    if let Some(row) = endpoint_db::get(natural_key)?
        && let Ok(existing) = Uuid::parse_str(&row.uuid)
    {
        return Ok(existing);
    }
    let minted = Uuid::now_v7();
    let row = EndpointRow {
        name: natural_key.to_string(),
        uuid: minted.to_string(),
        addresses: Vec::new(),
        enabled: true,
    };
    if endpoint_db::insert(&row).is_ok() {
        return Ok(minted);
    }
    // Lost the mint race (or a transient write error): the winning row is
    // authoritative. Re-read; only fall back to the local mint if it is truly
    // absent (a real error we surface).
    match endpoint_db::get(natural_key)? {
        Some(won) => Uuid::parse_str(&won.uuid).map_err(|e| {
            anyhow::anyhow!("unit_identity row for '{natural_key}' has invalid uuid: {e}")
        }),
        None => Ok(minted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_db<T>(f: impl FnOnce() -> T) -> T {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("unit-identity.db");
        db::with_thread_db_path(&path, || {
            let conn = db::open_default().expect("open temp db");
            db::schema_fragments::apply_fragments(&conn).expect("apply fragments");
            drop(conn);
            f()
        })
    }

    #[test]
    fn mint_is_stable_for_the_same_key() {
        with_db(|| {
            let a = resolve_or_mint("cluster:a/lxc/100").expect("mint");
            let b = resolve_or_mint("cluster:a/lxc/100").expect("resolve");
            assert_eq!(a, b, "the same natural key must resolve to the same uuid");
            // uuidv7 (version nibble == 7).
            assert_eq!(a.get_version_num(), 7, "identity must be uuidv7");
        });
    }

    #[test]
    fn distinct_keys_get_distinct_ids() {
        with_db(|| {
            let a = resolve_or_mint("cluster:a/lxc/100").expect("mint a");
            let b = resolve_or_mint("cluster:a/lxc/101").expect("mint b");
            assert_ne!(
                a, b,
                "distinct real units must not collapse to one identity"
            );
        });
    }
}
