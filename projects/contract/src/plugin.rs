//! Plugin identity rules shared by the loader and the seams plugins register on.

/// Ids a plugin may never load as, and names no plugin may register a backend
/// or seam entry under. A plugin's principal owns every secret named `<id>` or
/// `<id>.…`, so an id equal to a core secret name (or a core namespace) would
/// hand the plugin that secret.
pub const RESERVED_PLUGIN_IDS: &[&str] = &[
    "orca",
    "core",
    "system",
    "auth",
    "secret",
    "secrets",
    "mesh",
    "model",
    "notify",
    "plugin",
    "db",
    "github",
    "github_token",
    "anthropic",
    "anthropic_api_key",
    "release_source_api",
    "ui",
];

pub fn is_reserved_plugin_id(id: &str) -> bool {
    RESERVED_PLUGIN_IDS.contains(&id)
}

/// Whether `id` matches `^[a-z][a-z0-9-]{0,63}$` with no trailing or doubled
/// `-`. No `.` (a dotted id would own a core secret's `<id>.…` names), no `/`
/// (the id becomes the install filename), and no `_` (`-` maps to `_` in db
/// namespaces, so `_` would let two ids collide).
pub fn is_valid_plugin_id(id: &str) -> bool {
    id.len() <= 64
        && id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b == b'-' || b.is_ascii_lowercase() || b.is_ascii_digit())
        && !id.ends_with('-')
        && !id.contains("--")
}
