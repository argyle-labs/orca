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
//! The policy is a per-host [`db::settings`] row (`remediation.policy`) and
//! defaults to [`RemediationPolicy::Notify`] when unset — the conservative
//! stance: orca never auto-acts until the operator opts in.

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Whether orca's self-healing controllers may act automatically on this host.
///
/// Variants order from most to least autonomous. `Default` is `Notify` (via
/// `#[default]`, so the derived impl satisfies the workspace `derivable_impls`
/// lint rather than a hand-written `impl Default`).
#[derive(
    Serialize, Deserialize, JsonSchema, clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default,
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
    #[default]
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

/// The subsystem a [`Remediator`](crate::remediation_controller::Remediator)
/// governs. The remediation policy resolves per-domain, so different classes of
/// self-heal can carry different autonomy without a single global switch.
///
/// A domain's [`default_policy`](RemediationDomain::default_policy) is the
/// stance used when the host sets no explicit override. Storage failover
/// (`NfsMount`) is mature enough to act-and-report by default; every other
/// domain defaults to the conservative `notify` until proven out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemediationDomain {
    /// Network-share (autofs / NFS) mount failover and stale-mount recovery.
    NfsMount,
    /// Service (container / unit) reconcile, wedge-break, and restart.
    Service,
}

impl RemediationDomain {
    /// Stable snake_case string used for the per-domain settings row and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NfsMount => "nfs_mount",
            Self::Service => "service",
        }
    }

    /// Per-domain settings key holding this domain's policy override.
    fn policy_key(&self) -> String {
        format!("{POLICY_KEY}.{}", self.as_str())
    }

    /// Policy for this domain when neither a per-domain nor a host-wide override
    /// is set. `NfsMount` acts-and-notifies (storage failover is proven);
    /// everything else stays `notify` — orca never auto-acts until opted in.
    pub fn default_policy(&self) -> RemediationPolicy {
        match self {
            Self::NfsMount => RemediationPolicy::AutoFixNotify,
            Self::Service => RemediationPolicy::Notify,
        }
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

/// Settings-table key holding this host's host-wide remediation policy. A
/// per-domain override lives at `remediation.policy.<domain>`.
pub const POLICY_KEY: &str = "remediation.policy";

/// Resolve the effective remediation policy for `domain` on this host.
///
/// Precedence, most specific first:
/// 1. the per-domain override (`remediation.policy.<domain>`),
/// 2. the host-wide override ([`POLICY_KEY`]),
/// 3. the domain's [`default_policy`](RemediationDomain::default_policy).
///
/// An unset or unparseable value at each layer falls through to the next.
pub fn policy(conn: &db::Conn, domain: RemediationDomain) -> anyhow::Result<RemediationPolicy> {
    if let Some(p) = db::settings::get(conn, &domain.policy_key())?.and_then(|v| v.parse().ok()) {
        return Ok(p);
    }
    if let Some(p) = host_override(conn)? {
        return Ok(p);
    }
    Ok(domain.default_policy())
}

/// The host-wide remediation override ([`POLICY_KEY`]), if the operator set a
/// parseable one. `None` means "no host-wide override — use the per-domain
/// default".
pub fn host_override(conn: &db::Conn) -> anyhow::Result<Option<RemediationPolicy>> {
    Ok(db::settings::get(conn, POLICY_KEY)?.and_then(|v| v.parse().ok()))
}

// ── Tools: system.remediation.{get,set} ──────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct RemediationGetArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemediationGetOutput {
    pub policy: RemediationPolicy,
}

/// Report this host's self-heal remediation policy. Defaults to `notify` when
/// the operator has not set one.
#[orca_tool(domain = "system", verb = "remediation.get")]
async fn remediation_get(
    _args: RemediationGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RemediationGetOutput> {
    let conn = db::open_default()?;
    Ok(RemediationGetOutput {
        policy: host_override(&conn)?.unwrap_or_default(),
    })
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RemediationSetArgs {
    /// New remediation policy for this host.
    #[arg(long, value_enum)]
    pub policy: RemediationPolicy,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemediationSetOutput {
    pub policy: RemediationPolicy,
}

/// Set this host's self-heal remediation policy. Governs whether orca's
/// self-healing controllers act automatically, propose the action for approval,
/// or stay silent.
#[orca_tool(domain = "system", verb = "remediation.set")]
async fn remediation_set(
    args: RemediationSetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RemediationSetOutput> {
    let conn = db::open_default()?;
    db::settings::set(&conn, POLICY_KEY, args.policy.as_str())?;
    Ok(RemediationSetOutput {
        policy: args.policy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_notify() {
        assert_eq!(RemediationPolicy::default(), RemediationPolicy::Notify);
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
    fn str_roundtrips() {
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
    fn serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&RemediationPolicy::AutoFixNotify).unwrap(),
            "\"auto_fix_notify\""
        );
    }

    #[test]
    fn policy_falls_back_to_per_domain_default_when_unset() {
        let conn = db::testing::test_conn();
        // No override → each domain's own default.
        assert_eq!(
            policy(&conn, RemediationDomain::NfsMount).unwrap(),
            RemediationPolicy::AutoFixNotify
        );
        assert_eq!(
            policy(&conn, RemediationDomain::Service).unwrap(),
            RemediationPolicy::Notify
        );
    }

    #[test]
    fn host_override_applies_to_every_domain() {
        let conn = db::testing::test_conn();
        db::settings::set(&conn, POLICY_KEY, RemediationPolicy::Disabled.as_str()).unwrap();
        assert_eq!(
            policy(&conn, RemediationDomain::NfsMount).unwrap(),
            RemediationPolicy::Disabled
        );
        assert_eq!(
            policy(&conn, RemediationDomain::Service).unwrap(),
            RemediationPolicy::Disabled
        );
    }

    #[test]
    fn per_domain_override_beats_host_override() {
        let conn = db::testing::test_conn();
        db::settings::set(&conn, POLICY_KEY, RemediationPolicy::Disabled.as_str()).unwrap();
        db::settings::set(
            &conn,
            &RemediationDomain::Service.policy_key(),
            RemediationPolicy::AutoFix.as_str(),
        )
        .unwrap();
        assert_eq!(
            policy(&conn, RemediationDomain::Service).unwrap(),
            RemediationPolicy::AutoFix
        );
        // Untouched domain still follows the host-wide override.
        assert_eq!(
            policy(&conn, RemediationDomain::NfsMount).unwrap(),
            RemediationPolicy::Disabled
        );
    }
}
