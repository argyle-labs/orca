//! Claim identity — the stable orca UUIDv7 for each non-peer child a host
//! claims to run (docker container, proxmox vm/lxc, …).
//!
//! Hard rule: an id is a MINTED UUIDv7, never derived from an object's fields.
//! So the natural key `(provider, provider_instance, kind, native_id)` is a
//! set of **searchable attributes** used to find/correlate a claim — it maps
//! to a `uuid` that was minted once (via `utils::id::new()`) on first sight
//! and persisted here. The source peer (the one holding the provider creds)
//! owns the mint and reports the `uuid` on `TopologyClaim` so every viewer of
//! the tree agrees on one id per claim.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use utils::time::now_secs_since_epoch as now_secs;

/// Return the stable UUIDv7 for a claim's natural key, minting + persisting a
/// fresh one on first sight. Idempotent: the same natural key always resolves
/// to the same id for the life of this host's DB.
pub fn resolve_or_mint(
    conn: &Connection,
    provider: &str,
    provider_instance: &str,
    kind: &str,
    native_id: &str,
) -> Result<String> {
    if let Some(uuid) = conn
        .query_row(
            "SELECT uuid FROM claim_identity
             WHERE provider = ?1 AND provider_instance = ?2 AND kind = ?3 AND native_id = ?4",
            params![provider, provider_instance, kind, native_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(uuid);
    }
    let uuid = utils::id::new();
    // ON CONFLICT guards the race where two ticks mint concurrently: the first
    // write wins and we read it back rather than returning our losing value.
    conn.execute(
        "INSERT INTO claim_identity
            (provider, provider_instance, kind, native_id, uuid, minted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(provider, provider_instance, kind, native_id) DO NOTHING",
        params![
            provider,
            provider_instance,
            kind,
            native_id,
            uuid,
            now_secs()
        ],
    )?;
    let stored = conn.query_row(
        "SELECT uuid FROM claim_identity
         WHERE provider = ?1 AND provider_instance = ?2 AND kind = ?3 AND native_id = ?4",
        params![provider, provider_instance, kind, native_id],
        |r| r.get::<_, String>(0),
    )?;
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn mints_once_and_is_stable() {
        let conn = test_conn();
        let a = resolve_or_mint(&conn, "docker", "local", "container", "abc123").unwrap();
        let b = resolve_or_mint(&conn, "docker", "local", "container", "abc123").unwrap();
        assert_eq!(a, b, "same natural key resolves to the same id");
        assert!(utils::id::is_valid(&a), "id is a valid UUID");

        // A different native_id gets a distinct id.
        let c = resolve_or_mint(&conn, "docker", "local", "container", "def456").unwrap();
        assert_ne!(a, c);
    }
}
