//! Process-global lookup of `tool_name → required_role`. Populated once at
//! startup from `dispatch::role_table` so the REST middleware can gate
//! `/api/v1/*` without walking the inventory on every request.
//!
//! Sibling of `remote_ok`: same OnceLock pattern, different axis (per-caller
//! authorization vs peer-callable allowlist).

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

static ROLES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
static MUTATIONS: OnceLock<HashSet<&'static str>> = OnceLock::new();
static SECOND_FACTOR: OnceLock<HashSet<&'static str>> = OnceLock::new();
/// Plugin tools, installed at plugin load and removed at unload. The static
/// tables are link-time and frozen; plugins come and go at runtime.
static PLUGIN_TOOLS: RwLock<Option<HashMap<String, PluginToolPolicy>>> = RwLock::new(None);

/// Authorization policy a plugin declared for one of its tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginToolPolicy {
    pub role: &'static str,
    pub execute_gated: bool,
    pub dry_run_aware: bool,
    pub data_mutation: bool,
    pub local_only: bool,
    /// An `any` tool that explicitly opted out of the execute gate without
    /// also declaring `data_mutation: false`; mesh refuses it without a caller.
    pub read_unconfirmed: bool,
    pub tool_callable: bool,
    pub remote_admin: bool,
}

impl PluginToolPolicy {
    /// Policy from a plugin manifest entry. An absent or unrecognized role
    /// fails closed to `"admin"`: the manifest gives no proof the tool is
    /// read-only, and the mesh gate ranks unknown roles lowest, so passing one
    /// through would open the tool instead.
    pub fn from_manifest(
        name: &str,
        role: Option<&str>,
        execute_gated: Option<bool>,
        data_mutation: Option<bool>,
    ) -> Self {
        let role = match role {
            Some("any") => "any",
            Some("read") => "read",
            None | Some(_) => "admin",
        };
        let default_gated = role == "any" && !verb_shape::is_read_shaped(name);
        Self {
            role,
            execute_gated: execute_gated.unwrap_or(default_gated),
            dry_run_aware: false,
            data_mutation: data_mutation.unwrap_or(false),
            local_only: false,
            read_unconfirmed: role == "any"
                && execute_gated == Some(false)
                && data_mutation != Some(false),
            tool_callable: false,
            remote_admin: false,
        }
    }

    /// Apply the manifest's `local_only`. Absent means a plugin built before
    /// the flag crossed the wire, which stays remote-callable.
    pub fn local_only(mut self, local_only: Option<bool>) -> Self {
        self.local_only = local_only.unwrap_or(false);
        self
    }

    pub fn tool_callable(mut self, tool_callable: Option<bool>) -> Self {
        self.tool_callable = tool_callable.unwrap_or(false);
        self
    }

    pub fn remote_admin(mut self, remote_admin: Option<bool>) -> Self {
        self.remote_admin = remote_admin.unwrap_or(false);
        self
    }

    pub fn dry_run_aware(mut self, dry_run_aware: bool) -> Self {
        self.dry_run_aware = dry_run_aware;
        if dry_run_aware {
            self.role = "admin";
            self.read_unconfirmed = false;
        }
        self
    }
}

/// Install the policies of one plugin's tools, replacing any previous entry
/// under the same name.
pub fn install_plugin_tools(entries: impl IntoIterator<Item = (String, PluginToolPolicy)>) {
    let mut guard = PLUGIN_TOOLS.write().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashMap::new).extend(entries);
}

/// Drop the policies of unloaded plugin tools.
pub fn remove_plugin_tools<'a>(names: impl IntoIterator<Item = &'a str>) {
    let mut guard = PLUGIN_TOOLS.write().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        for n in names {
            map.remove(n);
        }
    }
}

/// True if a loaded plugin declared `tool` local-only.
pub fn is_plugin_local_only(tool: &str) -> bool {
    plugin_policy(tool).is_some_and(|p| p.local_only)
}

fn plugin_policy(tool: &str) -> Option<PluginToolPolicy> {
    PLUGIN_TOOLS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(tool).copied())
}

/// Whether a loaded plugin declared `tool` execute-gated, i.e. its own gate
/// plans on `dryRun` instead of applying.
pub fn plugin_execute_gated(tool: &str) -> bool {
    plugin_policy(tool).is_some_and(|p| p.execute_gated)
}

pub fn plugin_dry_run_aware(tool: &str) -> bool {
    plugin_policy(tool).is_some_and(|p| p.dry_run_aware)
}

pub fn plugin_dry_run_ok(tool: &str) -> bool {
    plugin_policy(tool).is_some_and(|p| p.execute_gated || p.dry_run_aware)
}

pub fn is_tool_callable(tool: &str) -> bool {
    crate::registry::tool_callable(tool) || plugin_policy(tool).is_some_and(|p| p.tool_callable)
}

/// Whether a loaded plugin's `any` tool lacks an explicit read declaration.
pub fn plugin_read_unconfirmed(tool: &str) -> bool {
    plugin_policy(tool).is_some_and(|p| p.read_unconfirmed)
}

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
        || plugin_policy(tool).is_some_and(|p| p.data_mutation)
        || crate::unit_surface::unit_policy(tool).is_some_and(|(_, m)| m)
}

/// Install the second-factor lookup. Idempotent — first call wins. Populated
/// once at startup from `dispatch::second_factor_names`, sibling to
/// [`install_mutations`].
pub fn install_second_factor(names: impl IntoIterator<Item = &'static str>) {
    let set: HashSet<&'static str> = names.into_iter().collect();
    _ = SECOND_FACTOR.set(set);
}

/// True if `tool` requires a verified second factor, independent of its role.
/// Only core `#[orca_tool]` tools can opt in today; plugin/unit/diagnostics/UPS
/// tools always return `false` (none of those surfaces has a way to declare it).
pub fn requires_fresh_second_factor(tool: &str) -> bool {
    SECOND_FACTOR.get().is_some_and(|s| s.contains(tool))
}

/// Role required to invoke `tool` over an authenticated surface: the core
/// table, then the role a loaded plugin declared for it, then the live unit
/// surface (mutating unit ops are `admin`). Returns `"any"`
/// for unknown tools and for tools registered before `install` ran — the
/// registry's own 404 path will reject unknown tool names downstream, so
/// fall-open here keeps the gate from double-handling missing-tool errors.
pub fn required_role(tool: &str) -> &'static str {
    known_role(tool).unwrap_or("any")
}

/// [`required_role`] without the fall-open: `None` exactly when the local
/// dispatcher cannot route `tool` (no core, plugin, unit, diagnostics or UPS
/// op under that name). Surfaces that must fail closed on an unrecognized
/// name key off this.
pub fn known_role(tool: &str) -> Option<&'static str> {
    ROLES
        .get()
        .and_then(|m| m.get(tool).copied())
        .or_else(|| plugin_policy(tool).map(|p| p.role))
        .or_else(|| crate::unit_surface::unit_policy(tool).map(|(role, _)| role))
        // Routable without an installed entry: the static table before
        // `install`, the fixed surfaces' `any` reads (only their admin pairs
        // are installed), and a loaded plugin tool with no installed policy,
        // which fails closed to admin.
        .or_else(|| crate::registry::required_role(tool))
        .or_else(|| fixed_surface_role(tool))
        .or_else(|| crate::registry::dynamic_owns(tool).then_some("admin"))
}

fn fixed_surface_role(tool: &str) -> Option<&'static str> {
    use crate::{diagnostics_surface as diag, ups_surface as ups};
    let pairs = if diag::diagnostics_owns(tool) {
        diag::diagnostics_role_pairs()
    } else if ups::ups_owns(tool) {
        ups::ups_role_pairs()
    } else {
        return None;
    };
    Some(
        pairs
            .into_iter()
            .find_map(|(n, r)| (n == tool).then_some(r))
            .unwrap_or("any"),
    )
}

pub fn is_remote_admin_safe(tool: &str) -> bool {
    crate::registry::remote_admin(tool) || plugin_policy(tool).is_some_and(|p| p.remote_admin)
}

pub fn is_custody(tool: &str) -> bool {
    !is_remote_admin_safe(tool)
}

/// Rank of a role in the tier order `read-only` < `user` < `admin` <
/// `super-admin`. `read` is `read-only` and `member` is `user`; `any` and
/// unknown roles rank lowest.
pub fn role_rank(role: &str) -> u8 {
    match role {
        "super-admin" => 4,
        "admin" => 3,
        "user" | "member" => 2,
        "read-only" | "read" => 1,
        _ => 0,
    }
}

/// True if the caller's identity-role satisfies the tool's required-role,
/// ranked by [`role_rank`]. `"read"` exists so sensitive read-only surfaces
/// (e.g. `fs.*`) can be gated above `"any"` without requiring admin. Unknown
/// required values fail closed, so a typo in `role = "..."` never falls open.
pub fn satisfies(caller_role: &str, required: &str) -> bool {
    let need = role_rank(required);
    (need > 0 || required == "any") && role_rank(caller_role) >= need
}

pub fn second_factor_ok(requires_second_factor: bool, second_factor: bool) -> bool {
    !requires_second_factor || second_factor
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
    fn role_rank_orders_the_four_tiers_above_any() {
        let ranks: Vec<u8> = ["any", "read-only", "user", "admin", "super-admin"]
            .into_iter()
            .map(role_rank)
            .collect();
        assert_eq!(ranks, [0, 1, 2, 3, 4]);
        assert_eq!(role_rank("read"), role_rank("read-only"));
        assert_eq!(role_rank("member"), role_rank("user"));
        assert_eq!(role_rank("bogus"), 0);
    }

    #[test]
    fn explicit_ungate_without_explicit_read_is_unconfirmed() {
        let p = |eg, dm| PluginToolPolicy::from_manifest("t.status", Some("any"), eg, dm);
        assert!(p(Some(false), None).read_unconfirmed);
        assert!(p(Some(false), Some(true)).read_unconfirmed);
        assert!(!p(Some(false), Some(false)).read_unconfirmed);
        assert!(!p(None, None).read_unconfirmed);
        assert!(
            !PluginToolPolicy::from_manifest("t.status", Some("admin"), Some(false), None)
                .read_unconfirmed
        );
    }

    #[test]
    fn satisfies_any_requirement_passes_every_caller() {
        assert!(satisfies("admin", "any"));
        assert!(satisfies("member", "any"));
        assert!(satisfies("", "any"));
    }

    #[test]
    fn second_factor_ok_is_independent_of_role() {
        assert!(!second_factor_ok(true, false));
        assert!(second_factor_ok(true, true));
        assert!(second_factor_ok(false, false));
    }

    #[test]
    fn super_admin_requirement_refuses_admin() {
        assert!(satisfies("super-admin", "super-admin"));
        assert!(!satisfies("admin", "super-admin"));
        assert!(!satisfies("member", "super-admin"));
    }

    #[test]
    fn satisfies_admin_requirement_passes_admin_and_super_admin() {
        assert!(satisfies("admin", "admin"));
        assert!(satisfies("super-admin", "admin"));
        assert!(!satisfies("member", "admin"));
        assert!(!satisfies("", "admin"));
    }

    #[test]
    fn satisfies_read_requirement_passes_read_and_above() {
        assert!(satisfies("admin", "read"));
        assert!(satisfies("read", "read"));
        assert!(satisfies("read-only", "read"));
        assert!(satisfies("member", "read"));
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
    fn every_routable_tool_is_known() {
        let _registry = crate::UNIT_REGISTRY_LOCK.blocking_lock();
        for def in crate::mcp_definitions() {
            let name = def["name"].as_str().expect("tool def has a name");
            assert!(known_role(name).is_some(), "{name} routes but is unknown");
        }
        assert_eq!(
            known_role(crate::diagnostics_surface::DIAGNOSE_TOOL),
            Some("any")
        );
        assert_eq!(known_role(crate::ups_surface::STATE_TOOL), Some("any"));
        assert_eq!(known_role(crate::ups_surface::CONFIG_TOOL), Some("any"));
        assert_eq!(
            known_role(crate::diagnostics_surface::REPAIR_TOOL),
            Some("admin")
        );
        assert_eq!(
            known_role(crate::ups_surface::CONFIGURE_TOOL),
            Some("admin")
        );
        assert_eq!(known_role("__no_such_tool__"), None);
    }

    #[test]
    fn plugin_policy_maps_manifest_roles_and_fails_closed() {
        let p = |r| PluginToolPolicy::from_manifest("t.list", r, None, None).role;
        assert_eq!(p(None), "admin");
        assert_eq!(p(Some("any")), "any");
        assert_eq!(p(Some("read")), "read");
        assert_eq!(p(Some("admin")), "admin");
        assert_eq!(p(Some("wizard")), "admin");
    }

    #[test]
    fn plugin_local_only_is_installed_and_removed() {
        let name = "tool_roles_test_plugin.engine_update";
        let policy = PluginToolPolicy::from_manifest(name, Some("admin"), None, None);
        assert!(!policy.local_only);
        assert!(!policy.local_only(None).local_only);
        install_plugin_tools([(name.to_string(), policy.local_only(Some(true)))]);
        assert!(is_plugin_local_only(name));
        assert!(!crate::remote_ok::is_allowed(name));
        remove_plugin_tools([name]);
        assert!(!is_plugin_local_only(name));
        assert!(crate::remote_ok::is_allowed(name));
    }

    #[test]
    fn plugin_tools_install_and_remove_drive_required_role() {
        let name = "tool_roles_test_plugin.adopt";
        assert_eq!(required_role(name), "any");
        install_plugin_tools([(
            name.to_string(),
            PluginToolPolicy::from_manifest(name, Some("admin"), Some(true), Some(true)),
        )]);
        assert_eq!(required_role(name), "admin");
        assert!(is_data_mutation(name));
        assert!(!authorize("member", false, required_role(name), true));
        assert!(authorize("admin", false, required_role(name), true));
        remove_plugin_tools([name]);
        assert_eq!(required_role(name), "any");
        assert!(!is_data_mutation(name));
    }

    #[test]
    fn undeclared_plugin_tool_is_admin_only() {
        let name = "tool_roles_test_plugin.no_role";
        install_plugin_tools([(
            name.to_string(),
            PluginToolPolicy::from_manifest(name, None, None, Some(false)),
        )]);
        assert_eq!(required_role(name), "admin");
        assert!(!authorize("member", false, required_role(name), false));
        assert!(!authorize("read", false, required_role(name), false));
        assert!(authorize("admin", false, required_role(name), false));
        remove_plugin_tools([name]);
    }

    #[test]
    fn declared_read_plugin_tool_admits_read_caller() {
        let name = "tool_roles_test_plugin.read_role";
        install_plugin_tools([(
            name.to_string(),
            PluginToolPolicy::from_manifest(name, Some("read"), None, None),
        )]);
        assert!(authorize("read", false, required_role(name), false));
        assert!(authorize("member", false, required_role(name), false));
        assert!(!authorize("any", false, required_role(name), false));
        remove_plugin_tools([name]);
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
