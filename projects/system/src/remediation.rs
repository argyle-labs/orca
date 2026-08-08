//! Per-system remediation policy — the gate that governs whether orca's
//! self-healing controllers may take automatic, state-changing action.
//!
//! The health *view* stays a pure read; this policy sits on the *controller*
//! path only. A controller (today the storage self-heal loop; container
//! wedge/reconcile is a planned second consumer) consults the local host's
//! policy at the point of a would-be remediation and either acts, proposes the
//! action for approval, or stays silent:
//!
//! * `auto_fix`        — remediate automatically, silently.
//! * `auto_fix_notify` — remediate automatically AND emit a notification of
//!   what was done.
//! * `notify`          — do NOT act; emit a dismissable notification carrying
//!   the PROPOSED remediation action for the operator to approve.
//! * `disabled`        — do not act and do not notify (silent).
//!
//! The policy is resolved **per remediation domain** ([`RemediationDomain`]).
//! Each domain has a conservative built-in default; an operator may store an
//! explicit per-host override. Resolution order for the effective policy is:
//! explicit per-host override for that domain (if set) → that domain's built-in
//! default. Overrides live in per-domain [`db::settings`] rows keyed
//! `remediation.policy.<domain>` (e.g. `remediation.policy.nfs_mount`).
//!
//! Built-in defaults are domain-dependent:
//!
//! * [`RemediationDomain::NfsMount`] → [`RemediationPolicy::AutoFixNotify`] —
//!   storage failover auto-acts (as it did before the gate existed) and
//!   surfaces what it did.
//! * [`RemediationDomain::Service`] (and any other domain) →
//!   [`RemediationPolicy::Notify`] — the conservative default: orca never
//!   auto-acts on a service until the operator opts in.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A remediation domain — the class of self-heal action a policy governs. Each
/// domain resolves independently and carries its own conservative built-in
/// default (see [`RemediationDomain::default_policy`]). Extensible: new
/// controllers add a variant.
#[derive(
    Serialize, Deserialize, JsonSchema, clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum RemediationDomain {
    /// NFS / network-share mount recovery and source failover (storage
    /// self-heal). Defaults to `auto_fix_notify`.
    #[value(name = "nfs_mount")]
    NfsMount,
    /// General service restart / reconcile — the conservative default bucket.
    /// Defaults to `notify`.
    #[value(name = "service")]
    #[default]
    Service,
}

impl RemediationDomain {
    /// Stable snake_case string used for the settings key suffix and log lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NfsMount => "nfs_mount",
            Self::Service => "service",
        }
    }

    /// The built-in default policy for this domain, applied when no explicit
    /// per-host override is set. Domain-dependent: `NfsMount` auto-acts (and
    /// notifies); every other domain stays conservative (`notify`).
    pub fn default_policy(&self) -> RemediationPolicy {
        match self {
            Self::NfsMount => RemediationPolicy::AutoFixNotify,
            Self::Service => RemediationPolicy::Notify,
        }
    }

    /// The per-host settings-table key holding this domain's explicit override,
    /// e.g. `remediation.policy.nfs_mount`.
    pub fn policy_key(&self) -> String {
        format!("{POLICY_KEY_PREFIX}.{}", self.as_str())
    }
}

impl std::str::FromStr for RemediationDomain {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "nfs_mount" => Ok(Self::NfsMount),
            "service" => Ok(Self::Service),
            other => Err(format!("unknown remediation domain `{other}`")),
        }
    }
}

/// Whether orca's self-healing controllers may act automatically on this host.
///
/// Variants order from most to least autonomous. The effective policy for a
/// given [`RemediationDomain`] is resolved via [`policy`]; the built-in default
/// is domain-dependent ([`RemediationDomain::default_policy`]), so this enum has
/// no meaningful global `Default`.
#[derive(
    Serialize, Deserialize, JsonSchema, clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum RemediationPolicy {
    /// Controller remediates automatically, silently.
    #[value(name = "auto_fix")]
    AutoFix,
    /// Controller remediates automatically AND emits a notification of what was done.
    #[value(name = "auto_fix_notify")]
    AutoFixNotify,
    /// Controller does NOT act; emits a dismissable notification carrying the
    /// PROPOSED remediation action for user approval.
    #[value(name = "notify")]
    Notify,
    /// Controller does not act and does not notify (silent).
    #[value(name = "disabled")]
    Disabled,
}

impl RemediationPolicy {
    /// Stable snake_case string used for the settings row and log lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoFix => "auto_fix",
            Self::AutoFixNotify => "auto_fix_notify",
            Self::Notify => "notify",
            Self::Disabled => "disabled",
        }
    }

    /// Whether the controller may take state-changing remediation action.
    pub fn acts(&self) -> bool {
        matches!(self, Self::AutoFix | Self::AutoFixNotify)
    }

    /// Whether the controller emits a notification — the proposed action when it
    /// does not act (`notify`), or a record of what it did (`auto_fix_notify`).
    pub fn notifies(&self) -> bool {
        matches!(self, Self::Notify | Self::AutoFixNotify)
    }
}

impl std::str::FromStr for RemediationPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto_fix" => Ok(Self::AutoFix),
            "auto_fix_notify" => Ok(Self::AutoFixNotify),
            "notify" => Ok(Self::Notify),
            "disabled" => Ok(Self::Disabled),
            other => Err(format!("unknown remediation policy `{other}`")),
        }
    }
}

/// Settings-key prefix for per-domain remediation overrides. The full key is
/// `remediation.policy.<domain>` (see [`RemediationDomain::policy_key`]).
pub const POLICY_KEY_PREFIX: &str = "remediation.policy";

/// The explicit per-host override for a domain, if the operator has set one and
/// it parses. Returns `None` when unset (so callers fall back to the domain
/// default) or when the stored value is unparseable.
pub fn policy_override(
    conn: &db::Conn,
    domain: RemediationDomain,
) -> anyhow::Result<Option<RemediationPolicy>> {
    Ok(db::settings::get(conn, &domain.policy_key())?.and_then(|v| v.parse().ok()))
}

/// Resolve the effective remediation policy for `domain`: the explicit per-host
/// override when set, otherwise the domain's built-in default
/// ([`RemediationDomain::default_policy`]).
pub fn policy(conn: &db::Conn, domain: RemediationDomain) -> anyhow::Result<RemediationPolicy> {
    Ok(policy_override(conn, domain)?.unwrap_or_else(|| domain.default_policy()))
}

// ── Tools: system.remediation.{get,set} ──────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct RemediationGetArgs {
    /// Remediation domain to resolve. Defaults to `service` when omitted.
    #[arg(long, value_enum, default_value = "service")]
    #[serde(default)]
    pub domain: RemediationDomain,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemediationGetOutput {
    /// The domain this policy governs.
    pub domain: RemediationDomain,
    /// The effective resolved policy: the explicit override when set, else the
    /// domain's built-in default.
    pub policy: RemediationPolicy,
    /// Whether `policy` is an explicit per-host override (`true`) or the domain
    /// built-in default (`false`).
    pub is_override: bool,
}

/// Report this host's self-heal remediation policy for a domain. Returns the
/// EFFECTIVE resolved policy — the explicit per-host override when set,
/// otherwise the domain's built-in default (`nfs_mount` → `auto_fix_notify`,
/// `service` → `notify`) — and whether it is an override or the default.
#[orca_tool(domain = "system", verb = "remediation.get")]
async fn remediation_get(
    args: RemediationGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RemediationGetOutput> {
    let conn = db::open_default()?;
    let ovr = policy_override(&conn, args.domain)?;
    Ok(RemediationGetOutput {
        domain: args.domain,
        policy: ovr.unwrap_or_else(|| args.domain.default_policy()),
        is_override: ovr.is_some(),
    })
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RemediationSetArgs {
    /// Remediation domain to override. Defaults to `service` when omitted.
    #[arg(long, value_enum, default_value = "service")]
    #[serde(default)]
    pub domain: RemediationDomain,
    /// New remediation policy override for this domain on this host.
    #[arg(long, value_enum)]
    pub policy: RemediationPolicy,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemediationSetOutput {
    /// The domain this override applies to.
    pub domain: RemediationDomain,
    /// The stored explicit override policy.
    pub policy: RemediationPolicy,
}

/// Set this host's self-heal remediation policy override for a domain. Stores an
/// explicit per-host override keyed `remediation.policy.<domain>`; governs
/// whether that domain's self-healing controllers act automatically, propose the
/// action for approval, or stay silent.
#[orca_tool(domain = "system", verb = "remediation.set")]
async fn remediation_set(
    args: RemediationSetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RemediationSetOutput> {
    let conn = db::open_default()?;
    db::settings::set(&conn, &args.domain.policy_key(), args.policy.as_str())?;
    Ok(RemediationSetOutput {
        domain: args.domain,
        policy: args.policy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_defaults_are_domain_dependent() {
        assert_eq!(
            RemediationDomain::NfsMount.default_policy(),
            RemediationPolicy::AutoFixNotify
        );
        assert_eq!(
            RemediationDomain::Service.default_policy(),
            RemediationPolicy::Notify
        );
    }

    #[test]
    fn default_domain_is_service() {
        assert_eq!(RemediationDomain::default(), RemediationDomain::Service);
    }

    #[test]
    fn acts_and_notifies_matrix() {
        assert!(RemediationPolicy::AutoFix.acts());
        assert!(!RemediationPolicy::AutoFix.notifies());
        assert!(RemediationPolicy::AutoFixNotify.acts());
        assert!(RemediationPolicy::AutoFixNotify.notifies());
        assert!(!RemediationPolicy::Notify.acts());
        assert!(RemediationPolicy::Notify.notifies());
        assert!(!RemediationPolicy::Disabled.acts());
        assert!(!RemediationPolicy::Disabled.notifies());
    }

    #[test]
    fn policy_str_roundtrips() {
        for p in [
            RemediationPolicy::AutoFix,
            RemediationPolicy::AutoFixNotify,
            RemediationPolicy::Notify,
            RemediationPolicy::Disabled,
        ] {
            assert_eq!(p.as_str().parse::<RemediationPolicy>().unwrap(), p);
        }
        assert!("bogus".parse::<RemediationPolicy>().is_err());
    }

    #[test]
    fn domain_str_roundtrips() {
        for d in [RemediationDomain::NfsMount, RemediationDomain::Service] {
            assert_eq!(d.as_str().parse::<RemediationDomain>().unwrap(), d);
        }
        assert!("bogus".parse::<RemediationDomain>().is_err());
    }

    #[test]
    fn policy_key_is_per_domain() {
        assert_eq!(
            RemediationDomain::NfsMount.policy_key(),
            "remediation.policy.nfs_mount"
        );
        assert_eq!(
            RemediationDomain::Service.policy_key(),
            "remediation.policy.service"
        );
    }

    #[test]
    fn serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&RemediationPolicy::AutoFixNotify).unwrap(),
            "\"auto_fix_notify\""
        );
        assert_eq!(
            serde_json::to_string(&RemediationDomain::NfsMount).unwrap(),
            "\"nfs_mount\""
        );
    }

    #[test]
    fn resolves_domain_default_when_unset() {
        let conn = db::testing::test_conn();
        // No override → each domain resolves to its built-in default.
        assert_eq!(
            policy(&conn, RemediationDomain::NfsMount).unwrap(),
            RemediationPolicy::AutoFixNotify
        );
        assert_eq!(
            policy(&conn, RemediationDomain::Service).unwrap(),
            RemediationPolicy::Notify
        );
        assert!(
            policy_override(&conn, RemediationDomain::NfsMount)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_override_wins_over_domain_default() {
        let conn = db::testing::test_conn();
        db::settings::set(
            &conn,
            &RemediationDomain::NfsMount.policy_key(),
            RemediationPolicy::Notify.as_str(),
        )
        .unwrap();
        // Override wins for the set domain...
        assert_eq!(
            policy(&conn, RemediationDomain::NfsMount).unwrap(),
            RemediationPolicy::Notify
        );
        assert_eq!(
            policy_override(&conn, RemediationDomain::NfsMount).unwrap(),
            Some(RemediationPolicy::Notify)
        );
        // ...and does not leak to another domain, which still uses its default.
        assert_eq!(
            policy(&conn, RemediationDomain::Service).unwrap(),
            RemediationPolicy::Notify
        );
        assert!(
            policy_override(&conn, RemediationDomain::Service)
                .unwrap()
                .is_none()
        );
    }
}
