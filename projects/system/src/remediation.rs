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

/// Settings-table key holding this host's remediation policy.
pub const POLICY_KEY: &str = "remediation.policy";

/// Read the local host's remediation policy, defaulting to
/// [`RemediationPolicy::Notify`] when unset or unparseable (never auto-act
/// until the operator opts in).
pub fn policy(conn: &db::Conn) -> anyhow::Result<RemediationPolicy> {
    Ok(db::settings::get(conn, POLICY_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or_default())
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
        policy: policy(&conn)?,
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
    fn policy_defaults_when_unset_and_roundtrips_through_settings() {
        let conn = db::testing::test_conn();
        assert_eq!(policy(&conn).unwrap(), RemediationPolicy::Notify);
        db::settings::set(&conn, POLICY_KEY, RemediationPolicy::AutoFix.as_str()).unwrap();
        assert_eq!(policy(&conn).unwrap(), RemediationPolicy::AutoFix);
    }
}
