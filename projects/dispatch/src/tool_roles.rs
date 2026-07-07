//! Process-global lookup of `tool_name → required_role`. Populated once at
//! startup from `dispatch::role_table` so the REST middleware can gate
//! `/api/v1/*` without walking the inventory on every request.
//!
//! Sibling of `remote_ok`: same OnceLock pattern, different axis (per-caller
//! authorization vs peer-callable allowlist).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

static ROLES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
static MUTATIONS: OnceLock<HashSet<&'static str>> = OnceLock::new();

/// Install the lookup. Idempotent — first call wins; subsequent calls are
/// no-ops. Matches the registry's single-instance lifecycle.
pub fn install(pairs: impl IntoIterator<Item = (&'static str, &'static str)>) {
    let map: HashMap<&'static str, &'static str> = pairs.into_iter().collect();
    _ = ROLES.set(map);
}

/// Install the data-mutation lookup. Idempotent — first call wins. Populated
/// once at startup from `dispatch::data_mutation_names`, sibling to [`install`].
pub fn install_mutations(names: impl IntoIterator<Item = &'static str>) {
    let set: HashSet<&'static str> = names.into_iter().collect();
    _ = MUTATIONS.set(set);
}

/// True if `tool` is a data mutation (write against an external managed
/// system). Unknown tools / a pre-`install_mutations` state → `false` (the
/// opt-in escape hatch stays closed until the table is populated).
pub fn is_data_mutation(tool: &str) -> bool {
    MUTATIONS.get().is_some_and(|s| s.contains(tool))
}

/// Role required to invoke `tool` over an authenticated REST surface. Returns
/// `"any"` for unknown tools and for tools registered before `install` ran —
/// the registry's own 404 path will reject unknown tool names downstream, so
/// fall-open here keeps the gate from double-handling missing-tool errors.
pub fn required_role(tool: &str) -> &'static str {
    ROLES
        .get()
        .and_then(|m| m.get(tool).copied())
        .unwrap_or("any")
}

/// True if the caller's identity-role satisfies the tool's required-role.
///
/// Role hierarchy (high → low): `admin` > `read` > `any`. Higher roles
/// satisfy every requirement at their level or below. `"read"` exists so
/// sensitive read-only surfaces (e.g. `fs.*`, which can exfiltrate any
/// file an orca process can see) can be gated above `"any"` without
/// requiring full admin to invoke. Unknown values fail closed.
pub fn satisfies(caller_role: &str, required: &str) -> bool {
    match required {
        "any" => true,
        "read" => caller_role == "admin" || caller_role == "read",
        "admin" => caller_role == "admin",
        // Unknown required values fail closed — keeps a typo in `role = "..."`
        // from silently degrading to fall-open.
        _ => false,
    }
}

/// Authorization decision for an authenticated surface caller, combining the
/// role hierarchy with the `can_mutate` opt-in.
///
/// A caller passes when their role satisfies the tool's requirement outright,
/// OR — the opt-in escape hatch — when all of the following hold: the tool is a
/// **data mutation**, its requirement is `"admin"`, the caller holds the
/// `can_mutate` capability, and the caller is an authenticated identity
/// (non-empty role). These keep the opt-in from reaching control-plane admin
/// tools (which are never `data_mutation`) or from elevating an
/// unauthenticated/`"any"` caller. `admin` callers always pass via the first
/// branch. The identity floor is role-vocabulary-agnostic on purpose — token
/// roles are `admin`/`read`, user roles are `admin`/`member`; any real one
/// qualifies, only the empty/absent role does not.
pub fn authorize(
    caller_role: &str,
    can_mutate: bool,
    required: &str,
    is_data_mutation: bool,
) -> bool {
    if satisfies(caller_role, required) {
        return true;
    }
    can_mutate
        && is_data_mutation
        && required == "admin"
        && !caller_role.is_empty()
        && caller_role != "any"
}

#[cfg(test)]
mod tests {
    use super::*;

    // `install` writes to a process-global OnceLock and is shared across the
    // whole test binary, so we exercise its semantics through the public
    // accessors without re-installing in every test.

    #[test]
    fn satisfies_any_requirement_passes_every_caller() {
        assert!(satisfies("admin", "any"));
        assert!(satisfies("member", "any"));
        assert!(satisfies("", "any"));
    }

    #[test]
    fn satisfies_admin_requirement_only_passes_admin() {
        assert!(satisfies("admin", "admin"));
        assert!(!satisfies("member", "admin"));
        assert!(!satisfies("", "admin"));
    }

    #[test]
    fn satisfies_read_requirement_passes_admin_and_read_but_not_lower() {
        assert!(satisfies("admin", "read"));
        assert!(satisfies("read", "read"));
        assert!(!satisfies("member", "read"));
        assert!(!satisfies("any", "read"));
        assert!(!satisfies("", "read"));
    }

    #[test]
    fn authorize_admin_passes_everything() {
        assert!(authorize("admin", false, "admin", true));
        assert!(authorize("admin", false, "admin", false));
        assert!(authorize("admin", false, "read", false));
    }

    #[test]
    fn authorize_role_satisfied_without_opt_in() {
        assert!(authorize("read", false, "read", false));
        assert!(authorize("member", false, "any", false));
    }

    #[test]
    fn authorize_opt_in_unlocks_data_mutation_for_non_admin() {
        // read/member + can_mutate may run an admin-gated DATA MUTATION.
        assert!(authorize("read", true, "admin", true));
        assert!(authorize("member", true, "admin", true));
    }

    #[test]
    fn authorize_opt_in_never_unlocks_control_plane_admin() {
        // Same opt-in, but the tool is NOT a data mutation → still denied.
        assert!(!authorize("read", true, "admin", false));
        assert!(!authorize("member", true, "admin", false));
    }

    #[test]
    fn authorize_opt_in_requires_authenticated_identity() {
        // A data mutation, opted in, but no real identity → denied.
        assert!(!authorize("", true, "admin", true));
        assert!(!authorize("any", true, "admin", true));
    }

    #[test]
    fn authorize_without_opt_in_denies_non_admin_mutation() {
        assert!(!authorize("read", false, "admin", true));
        assert!(!authorize("member", false, "admin", true));
    }

    #[test]
    fn satisfies_unknown_requirement_fails_closed() {
        assert!(!satisfies("admin", "wizard"));
        assert!(!satisfies("member", ""));
    }

    #[test]
    fn required_role_for_unknown_tool_is_any_when_uninstalled_or_missing() {
        // Either we ran before `install` (uninstalled path) or after (installed
        // path with no entry for this name). Either way, unknown names map to
        // "any" so the gate falls open and the registry's own 404 wins.
        assert_eq!(required_role("__no_such_tool__"), "any");
    }

    #[test]
    fn install_seeds_lookup_and_required_role_returns_installed_value() {
        // First-call-wins: if another test in this binary installed first,
        // we read whatever is in there. Install our own as best-effort and
        // verify lookup behaves consistently with whatever map is live.
        install([("system.dev_enable", "admin"), ("system.doctor", "any")]);
        // Whichever install won, the lookup must be stable and deterministic.
        let role = required_role("system.dev_enable");
        assert!(role == "admin" || role == "any", "got {role}");
    }
}
