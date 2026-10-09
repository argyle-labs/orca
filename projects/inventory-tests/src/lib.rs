// Hosts the integration tests in `tests/` that link every #[orca_tool] bucket
// and walk the inventory, plus the schema predicates they share.

#![allow(clippy::disallowed_types)] // inspects the registry's dynamic JSON schemas

use serde_json::Value;

/// A property that names an id: `id`, `*_id` or `*Id`.
pub fn is_id_name(name: &str) -> bool {
    name == "id" || name.ends_with("_id") || name.ends_with("Id")
}

/// Declared `format: uuid`, directly or as an array's items.
pub fn is_uuid_typed(prop: &Value) -> bool {
    prop.get("format").and_then(Value::as_str) == Some("uuid")
        || prop.get("items").is_some_and(is_uuid_typed)
}
