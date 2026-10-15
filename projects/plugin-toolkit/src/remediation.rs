//! Fail a tool with a structured way out. orca carries the [`Remediation`]
//! intact to every surface: REST JSON body, MCP `structuredContent`, CLI
//! numbered steps.

pub use contract::{Blocked, ManualStep, ManualSteps, Remediation};

/// A tool error saying the capability is unavailable and how to unblock it.
///
/// ```rust,ignore
/// return Err(blocked(
///     "unraid.vm_service_disabled",
///     "the VM service is disabled",
///     ManualSteps::new("Unraid exposes no API to enable the VM service")
///         .step("Enable VMs", "Settings → VM Manager → Enable VMs: Yes")
///         .verify("`orca vm list` returns without this error"),
/// ));
/// ```
pub fn blocked(
    reason_code: impl Into<String>,
    detail: impl Into<String>,
    remediation: impl Into<Remediation>,
) -> anyhow::Error {
    contract::OrcaError::blocked(reason_code, detail, remediation).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_error_downcasts_to_orca_error_with_steps() {
        let e = blocked(
            "x.off",
            "off",
            ManualSteps::new("no API")
                .step("Enable", "in the UI")
                .command("x enable")
                .verify("x status"),
        );
        let oe = e.downcast_ref::<contract::OrcaError>().unwrap();
        assert_eq!(oe.code.as_deref(), Some("x.off"));
        let Some(Remediation::Manual(m)) = oe.remediation.as_deref() else {
            panic!("expected manual steps")
        };
        assert_eq!(m.steps[0].command.as_deref(), Some("x enable"));
        assert_eq!(m.steps[0].verify.as_deref(), Some("x status"));
    }
}
