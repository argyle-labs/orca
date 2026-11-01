//! Role tier order shared by core dispatch and plugins, so both authorize with
//! one implementation.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_tiers_pass_admin() {
        assert!(satisfies("admin", "admin"));
        assert!(satisfies("super-admin", "admin"));
    }

    #[test]
    fn lower_tiers_fail_admin() {
        assert!(!satisfies("read", "admin"));
        assert!(!satisfies("member", "admin"));
    }

    #[test]
    fn unknown_roles_fail_closed() {
        assert!(!satisfies("bogus", "read"));
        assert!(!satisfies("super-admin", "bogus"));
        assert!(!satisfies("", "admin"));
    }
}
