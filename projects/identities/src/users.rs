//! User/account domain API. Callers work in domain terms (ids, usernames,
//! roles) and never see a database `Connection`; all storage access is
//! encapsulated behind the [`Db`](db::pool::Db) seam here, and the raw row CRUD
//! lives in the private `row` submodule.

use anyhow::Result;
use db::pool::Db;

mod row;

pub use row::{ReplicaUser, User, UserAuth};

/// Create a user. Returns the stored row. `id` is caller-minted (UUIDv7);
/// `updated_at` is stamped here.
pub fn insert(id: &str, username: &str, password_hash: &str, role: &str) -> Result<User> {
    let now = utils::time::now_rfc3339();
    Db::process().write(|c| row::insert(c, id, username, password_hash, role, &now))
}

/// Look up a user by id.
pub fn find_by_id(id: &str) -> Result<Option<User>> {
    Db::process().read(|c| row::find_by_id(c, id))
}

/// Look up a user's auth material by username (case-insensitive).
pub fn find_auth_by_username(username: &str) -> Result<Option<UserAuth>> {
    Db::process().read(|c| row::find_auth_by_username(c, username))
}

/// Replace a user's password hash. Returns true if a row was updated.
pub fn set_password_hash(id: &str, new_hash: &str) -> Result<bool> {
    let now = utils::time::now_rfc3339();
    Db::process().write(|c| row::set_password_hash(c, id, new_hash, &now))
}

/// Total user count.
pub fn count() -> Result<i64> {
    Db::process().read(row::count)
}

/// Count of users with the `admin` role.
pub fn count_admins() -> Result<i64> {
    Db::process().read(row::count_admins)
}

/// Delete a user by id. Returns true if a row was removed.
pub fn delete_by_id(id: &str) -> Result<bool> {
    Db::process().write(|c| row::delete_by_id(c, id))
}

/// `(id, username, role, updated_at)` tuples for every user.
pub fn list_full() -> Result<Vec<(String, String, String, String)>> {
    Db::process().read(row::list_full)
}

/// The first admin by creation order, if any.
pub fn first_admin() -> Result<Option<User>> {
    Db::process().read(row::first_admin)
}

/// All users.
pub fn list() -> Result<Vec<User>> {
    Db::process().read(row::list)
}
