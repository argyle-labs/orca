//! Claim-identity domain API: the stable UUIDv7 for each non-peer child a host
//! claims to run. Callers pass the natural key; DB access is encapsulated behind
//! the [`Db`](db::pool::Db) seam here, with the raw row logic in `row`.

use anyhow::Result;
use db::pool::Db;

mod row;

/// Return the stable UUIDv7 for a claim's natural key, minting + persisting a
/// fresh one on first sight. Idempotent: the same natural key always resolves to
/// the same id for the life of this host's DB.
pub fn resolve_or_mint(
    provider: &str,
    provider_instance: &str,
    kind: &str,
    native_id: &str,
) -> Result<String> {
    Db::process().write(|c| row::resolve_or_mint(c, provider, provider_instance, kind, native_id))
}
