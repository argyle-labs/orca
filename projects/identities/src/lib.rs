//! `identities` — WHO exists in orca.
//!
//! Owns the identity storage that used to be split across `auth` and `db`:
//! user/account rows (mesh-replicated) and the claim-identity mint (a stable
//! UUIDv7 per non-peer child a host claims to run). The CREATE TABLE schema
//! stays in `db::apply_schema`; this crate owns the CRUD.
pub mod claim_identity;
pub mod users;
