//! Step-up: the extra proof an approval may need beyond a click.
//!
//! The requirement is policy (pure, here); the proof comes from a [`StepUp`]
//! provider that plugs in later (TOTP, WebAuthn, OIDC) without changing callers.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::risk::Risk;

/// Which factor satisfied a step-up; recorded on the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Factor {
    None,
    Totp,
    Webauthn,
    Oidc,
}

impl Factor {
    pub fn as_str(self) -> &'static str {
        match self {
            Factor::None => "none",
            Factor::Totp => "totp",
            Factor::Webauthn => "webauthn",
            Factor::Oidc => "oidc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Factor::None),
            "totp" => Some(Factor::Totp),
            "webauthn" => Some(Factor::Webauthn),
            "oidc" => Some(Factor::Oidc),
            _ => None,
        }
    }
}

/// Org-wide policy, set by a super-admin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequireTwoFactor {
    #[default]
    Off,
    /// Every user must enroll; does not by itself demand step-up.
    Enroll,
    /// Step-up for high-risk approvals.
    StepUpHigh,
    /// Step-up for every approval and every medium-or-higher verb.
    StepUpAll,
}

/// A user's own step-up preferences. Ignored until they have enrolled, and it
/// can only add friction for that user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserTwoFactor {
    pub enrolled: bool,
    /// Step-up for verbs at or above this risk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_up_from_risk: Option<Risk>,
    /// Step-up for these tools regardless of risk.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub step_up_tools: BTreeSet<String>,
}

/// Ordered: the effective requirement is the maximum of its sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepUpRequirement {
    None,
    /// An explicit approval, but no extra factor.
    Approval,
    StepUp,
}

pub fn requirement(
    risk: Risk,
    tool: &str,
    org: RequireTwoFactor,
    user: &UserTwoFactor,
) -> StepUpRequirement {
    let from_risk = if risk >= Risk::High {
        StepUpRequirement::Approval
    } else {
        StepUpRequirement::None
    };
    let from_org = match org {
        RequireTwoFactor::Off | RequireTwoFactor::Enroll => StepUpRequirement::None,
        RequireTwoFactor::StepUpHigh if risk >= Risk::High => StepUpRequirement::StepUp,
        RequireTwoFactor::StepUpAll if risk >= Risk::Medium => StepUpRequirement::StepUp,
        RequireTwoFactor::StepUpHigh | RequireTwoFactor::StepUpAll => StepUpRequirement::None,
    };
    let from_user = if user.enrolled
        && (user.step_up_from_risk.is_some_and(|min| risk >= min)
            || user.step_up_tools.contains(tool))
    {
        StepUpRequirement::StepUp
    } else {
        StepUpRequirement::None
    };
    from_risk.max(from_org).max(from_user)
}

/// Who is trying to decide an approval or present a factor. Only a bound user
/// session is a person; the other classes can never satisfy a step-up or
/// approve a high-risk request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalCaller {
    User { user_id: String },
    ServiceToken,
    UnboundToken,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepUpRefusal {
    /// The caller class may not decide this approval at all.
    CallerNotPermitted,
    /// A factor is required but this provider cannot supply one.
    FactorUnavailable,
    /// The presented proof did not verify.
    ProofRejected,
}

impl std::fmt::Display for StepUpRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            StepUpRefusal::CallerNotPermitted => "caller may not decide this approval",
            StepUpRefusal::FactorUnavailable => {
                "no second factor provider can satisfy this step-up"
            }
            StepUpRefusal::ProofRejected => "second factor proof rejected",
        })
    }
}

impl std::error::Error for StepUpRefusal {}

impl ApprovalCaller {
    /// The user id allowed to decide an approval or present a factor. Service
    /// tokens, unbound tokens and system callers never qualify.
    pub fn approver(&self) -> Result<&str, StepUpRefusal> {
        match self {
            ApprovalCaller::User { user_id } => Ok(user_id),
            _ => Err(StepUpRefusal::CallerNotPermitted),
        }
    }
}

pub trait StepUp: Send + Sync {
    fn factor(&self) -> Factor;

    /// Check `proof` for `user_id`. Called only once the caller has been
    /// established as a bound user; use [`verify`] rather than calling directly.
    fn check(&self, user_id: &str, proof: Option<&str>) -> Result<Factor, StepUpRefusal>;
}

/// Satisfy `required` for `caller`. The caller-class check runs before any
/// provider, so a provider cannot forget it.
pub fn verify(
    provider: &dyn StepUp,
    caller: &ApprovalCaller,
    required: StepUpRequirement,
    proof: Option<&str>,
) -> Result<Factor, StepUpRefusal> {
    let user_id = caller.approver()?;
    if required < StepUpRequirement::StepUp {
        return Ok(Factor::None);
    }
    match provider.check(user_id, proof)? {
        Factor::None => Err(StepUpRefusal::FactorUnavailable),
        factor => Ok(factor),
    }
}

/// The default provider: an approval click is enough, so it can satisfy no
/// step-up.
pub struct NoneProvider;

impl StepUp for NoneProvider {
    fn factor(&self) -> Factor {
        Factor::None
    }

    fn check(&self, _user_id: &str, _proof: Option<&str>) -> Result<Factor, StepUpRefusal> {
        Err(StepUpRefusal::FactorUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RISKS: [Risk; 3] = [Risk::Low, Risk::Medium, Risk::High];
    const ORGS: [RequireTwoFactor; 4] = [
        RequireTwoFactor::Off,
        RequireTwoFactor::Enroll,
        RequireTwoFactor::StepUpHigh,
        RequireTwoFactor::StepUpAll,
    ];

    fn user(enrolled: bool, from: Option<Risk>) -> UserTwoFactor {
        UserTwoFactor {
            enrolled,
            step_up_from_risk: from,
            step_up_tools: BTreeSet::new(),
        }
    }

    #[test]
    fn org_policy_table() {
        use StepUpRequirement::*;
        let u = UserTwoFactor::default();
        let expect = |org, risk| match (org, risk) {
            (RequireTwoFactor::StepUpHigh, Risk::High) => StepUp,
            (RequireTwoFactor::StepUpAll, Risk::Medium | Risk::High) => StepUp,
            (_, Risk::High) => Approval,
            _ => None,
        };
        for org in ORGS {
            for risk in RISKS {
                assert_eq!(
                    requirement(risk, "t.x", org, &u),
                    expect(org, risk),
                    "{org:?} {risk:?}"
                );
            }
        }
    }

    #[test]
    fn user_setting_only_applies_once_enrolled() {
        for risk in RISKS {
            let off = requirement(
                risk,
                "t.x",
                RequireTwoFactor::Off,
                &user(false, Some(Risk::Low)),
            );
            assert!(off < StepUpRequirement::StepUp, "{risk:?}");
            let on = requirement(
                risk,
                "t.x",
                RequireTwoFactor::Off,
                &user(true, Some(Risk::Low)),
            );
            assert_eq!(on, StepUpRequirement::StepUp, "{risk:?}");
        }
    }

    #[test]
    fn user_threshold_gates_at_or_above() {
        let u = user(true, Some(Risk::Medium));
        let got: Vec<_> = RISKS
            .iter()
            .map(|r| requirement(*r, "t.x", RequireTwoFactor::Off, &u))
            .collect();
        assert_eq!(
            got,
            [
                StepUpRequirement::None,
                StepUpRequirement::StepUp,
                StepUpRequirement::StepUp
            ]
        );
    }

    #[test]
    fn user_can_gate_a_specific_tool() {
        let mut u = user(true, None);
        u.step_up_tools.insert("garage.bucket_delete".into());
        assert_eq!(
            requirement(Risk::Low, "garage.bucket_delete", RequireTwoFactor::Off, &u),
            StepUpRequirement::StepUp
        );
        assert_eq!(
            requirement(Risk::Low, "garage.list", RequireTwoFactor::Off, &u),
            StepUpRequirement::None
        );
    }

    #[test]
    fn user_setting_never_lowers_org_policy() {
        let lax = user(true, None);
        for org in ORGS {
            for risk in RISKS {
                let with_user = requirement(risk, "t.x", org, &lax);
                let org_only = requirement(risk, "t.x", org, &UserTwoFactor::default());
                assert!(with_user >= org_only);
            }
        }
    }

    #[test]
    fn requirement_is_the_max_of_its_sources() {
        for org in ORGS {
            for risk in RISKS {
                for u in [
                    user(false, None),
                    user(true, Some(Risk::Low)),
                    user(true, Some(Risk::High)),
                ] {
                    let got = requirement(risk, "t.x", org, &u);
                    let org_only = requirement(risk, "t.x", org, &UserTwoFactor::default());
                    let user_only = requirement(risk, "t.x", RequireTwoFactor::Off, &u);
                    assert_eq!(got, org_only.max(user_only));
                }
            }
        }
    }

    fn non_users() -> [ApprovalCaller; 3] {
        [
            ApprovalCaller::ServiceToken,
            ApprovalCaller::UnboundToken,
            ApprovalCaller::System,
        ]
    }

    #[test]
    fn only_a_bound_user_can_decide_anything() {
        let u = ApprovalCaller::User {
            user_id: "u1".into(),
        };
        assert_eq!(u.approver(), Ok("u1"));
        for c in non_users() {
            assert_eq!(c.approver(), Err(StepUpRefusal::CallerNotPermitted));
        }
    }

    struct Always;
    impl StepUp for Always {
        fn factor(&self) -> Factor {
            Factor::Totp
        }
        fn check(&self, _: &str, _: Option<&str>) -> Result<Factor, StepUpRefusal> {
            Ok(Factor::Totp)
        }
    }

    #[test]
    fn a_permissive_provider_still_cannot_help_non_users() {
        for c in non_users() {
            for req in [StepUpRequirement::Approval, StepUpRequirement::StepUp] {
                assert_eq!(
                    verify(&Always, &c, req, Some("123456")),
                    Err(StepUpRefusal::CallerNotPermitted)
                );
            }
        }
    }

    #[test]
    fn none_provider_satisfies_approval_but_not_step_up() {
        let u = ApprovalCaller::User {
            user_id: "u1".into(),
        };
        assert_eq!(
            verify(&NoneProvider, &u, StepUpRequirement::Approval, None),
            Ok(Factor::None)
        );
        assert_eq!(
            verify(&NoneProvider, &u, StepUpRequirement::StepUp, None),
            Err(StepUpRefusal::FactorUnavailable)
        );
    }

    #[test]
    fn a_real_provider_satisfies_step_up_for_a_user() {
        let u = ApprovalCaller::User {
            user_id: "u1".into(),
        };
        assert_eq!(
            verify(&Always, &u, StepUpRequirement::StepUp, Some("1")),
            Ok(Factor::Totp)
        );
    }

    struct ReturnsNone;
    impl StepUp for ReturnsNone {
        fn factor(&self) -> Factor {
            Factor::None
        }
        fn check(&self, _: &str, _: Option<&str>) -> Result<Factor, StepUpRefusal> {
            Ok(Factor::None)
        }
    }

    #[test]
    fn a_provider_returning_no_factor_cannot_satisfy_step_up() {
        let u = ApprovalCaller::User {
            user_id: "u1".into(),
        };
        assert_eq!(
            verify(&ReturnsNone, &u, StepUpRequirement::StepUp, Some("1")),
            Err(StepUpRefusal::FactorUnavailable)
        );
        assert_eq!(
            verify(&ReturnsNone, &u, StepUpRequirement::Approval, None),
            Ok(Factor::None)
        );
        for c in non_users() {
            assert_eq!(
                verify(&ReturnsNone, &c, StepUpRequirement::StepUp, None),
                Err(StepUpRefusal::CallerNotPermitted)
            );
        }
    }

    #[test]
    fn serde_shapes() {
        assert_eq!(
            serde_json::to_string(&RequireTwoFactor::StepUpHigh).unwrap(),
            "\"step_up_high\""
        );
        for f in [Factor::None, Factor::Totp, Factor::Webauthn, Factor::Oidc] {
            assert_eq!(Factor::parse(f.as_str()), Some(f));
        }
    }
}
