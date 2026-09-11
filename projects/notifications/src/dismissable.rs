//! Dismissable notifications — the persistent, stateful notification domain.
//!
//! Distinct from the ephemeral [`Event`](crate::Event) dispatch: these are
//! raised, listed, dismissed and suppressed with a lifecycle, and persist. This
//! module is the domain API — callers work in domain terms ([`RaiseInput`],
//! keys, [`ListFilter`]) and never see a database `Connection`. All storage
//! access is encapsulated behind the [`Db`](db::pool::Db) seam here; the raw
//! row CRUD lives in the private `row` submodule.

use anyhow::Result;
use db::pool::Db;

mod row;

pub use row::{
    Audience, Fix, ListFilter, Notification, RaiseInput, Severity, State, derive_audience,
};

fn now_ms() -> i64 {
    utils::time::now().unix_millis()
}

/// Raise (create or reactivate) a notification. Idempotent on `input.key`.
pub fn raise(input: RaiseInput) -> Result<Notification> {
    Db::process().write(|c| row::raise(c, input, now_ms()))
}

/// Dismiss a notification by key. Returns the updated row, or `None` if absent.
pub fn dismiss(key: &str) -> Result<Option<Notification>> {
    Db::process().write(|c| row::dismiss(c, key, now_ms()))
}

/// Suppress a notification by key (re-raises become no-ops until reset).
pub fn suppress(key: &str) -> Result<Option<Notification>> {
    Db::process().write(|c| row::suppress(c, key, now_ms()))
}

/// Fetch a single notification by key.
pub fn get(key: &str) -> Result<Option<Notification>> {
    Db::process().read(|c| row::get(c, key))
}

/// List notifications matching `filter`, newest first.
pub fn list(filter: &ListFilter) -> Result<Vec<Notification>> {
    Db::process().read(|c| row::list(c, filter))
}
