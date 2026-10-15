//! How to unblock something orca cannot do on its own.
//!
//! A tool that hits a wall (no official API, a missing grant, a host setting
//! only the operator can flip) answers with a [`Remediation`]: either an
//! [`Automatic`](Remediation::Automatic) tool call orca can run, or
//! [`ManualSteps`] for the operator. It rides on [`OrcaError::remediation`] so
//! every surface carries it intact: REST as the JSON error body, MCP as
//! `structuredContent`, the CLI as numbered steps.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaError;
use crate::json_any::JsonAny;

/// One operator action.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManualStep {
    pub title: String,
    pub detail: String,
    /// A command to run, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The file the step edits or inspects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// How to confirm the step took effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
}

/// Ordered operator steps that unblock a capability orca cannot apply itself.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManualSteps {
    /// Why this is manual (e.g. "Unraid exposes no API for this setting").
    pub reason: String,
    pub steps: Vec<ManualStep>,
    /// What becomes possible once the steps are done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unblocks: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl ManualSteps {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            steps: Vec::new(),
            unblocks: None,
            warning: None,
        }
    }

    /// Append a step; `command`/`file`/`verify` then apply to it.
    pub fn step(mut self, title: impl Into<String>, detail: impl Into<String>) -> Self {
        self.steps.push(ManualStep {
            title: title.into(),
            detail: detail.into(),
            command: None,
            file: None,
            verify: None,
        });
        self
    }

    /// Panics if no step was added yet.
    pub fn command(mut self, command: impl Into<String>) -> Self {
        self.last().command = Some(command.into());
        self
    }

    /// Panics if no step was added yet.
    pub fn file(mut self, file: impl Into<String>) -> Self {
        self.last().file = Some(file.into());
        self
    }

    /// Panics if no step was added yet.
    pub fn verify(mut self, verify: impl Into<String>) -> Self {
        self.last().verify = Some(verify.into());
        self
    }

    pub fn unblocks(mut self, unblocks: impl Into<String>) -> Self {
        self.unblocks = Some(unblocks.into());
        self
    }

    pub fn warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(warning.into());
        self
    }

    fn last(&mut self) -> &mut ManualStep {
        self.steps
            .last_mut()
            .expect("ManualSteps: call step() before command/file/verify")
    }
}

/// The way out of a blocked state.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Remediation {
    /// A granted or declared op orca can run: invoke `tool` with `args`.
    Automatic {
        tool: String,
        args: JsonAny,
    },
    Manual(ManualSteps),
}

impl From<ManualSteps> for Remediation {
    fn from(steps: ManualSteps) -> Self {
        Self::Manual(steps)
    }
}

/// Renders for a terminal: numbered steps, indented specifics.
impl fmt::Display for Remediation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Automatic { tool, args } => {
                write!(f, "orca can fix this automatically: run `{tool}`")?;
                if !args.0.is_null() {
                    write!(f, " with {}", args.0)?;
                }
                Ok(())
            }
            Self::Manual(m) => {
                writeln!(f, "Manual steps required: {}", m.reason)?;
                if let Some(w) = &m.warning {
                    writeln!(f, "Warning: {w}")?;
                }
                for (i, s) in m.steps.iter().enumerate() {
                    writeln!(f, "  {}. {}", i + 1, s.title)?;
                    writeln!(f, "     {}", s.detail)?;
                    if let Some(file) = &s.file {
                        writeln!(f, "     file:    {file}")?;
                    }
                    if let Some(cmd) = &s.command {
                        writeln!(f, "     run:     {cmd}")?;
                    }
                    if let Some(v) = &s.verify {
                        writeln!(f, "     verify:  {v}")?;
                    }
                }
                if let Some(u) = &m.unblocks {
                    write!(f, "This unblocks: {u}")?;
                }
                Ok(())
            }
        }
    }
}

/// Success-path answer for a status/availability query whose capability is
/// unavailable. A failing call uses [`OrcaError::blocked`] instead.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Blocked {
    /// Always `false`; a single field for callers to branch on.
    pub available: bool,
    /// Stable machine-readable reason (e.g. `"unraid.vm_service_disabled"`).
    pub reason_code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<Remediation>,
}

impl Blocked {
    pub fn new(reason_code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            available: false,
            reason_code: reason_code.into(),
            detail: detail.into(),
            remediation: None,
        }
    }

    pub fn with_remediation(mut self, remediation: impl Into<Remediation>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }
}

impl From<Blocked> for OrcaError {
    fn from(b: Blocked) -> Self {
        let e = OrcaError::unavailable(b.detail).with_code(b.reason_code);
        match b.remediation {
            Some(r) => e.with_remediation(r),
            None => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ManualSteps {
        ManualSteps::new("no API exists for this setting")
            .step("Enable VMs", "Settings → VM Manager → Enable VMs: Yes")
            .file("/boot/config/domain.cfg")
            .verify("virsh list succeeds")
            .step("Restart", "Restart the VM service")
            .command("/etc/rc.d/rc.libvirt restart")
            .unblocks("vm.list")
            .warning("running VMs stop")
    }

    #[test]
    fn builder_attaches_specifics_to_the_last_step() {
        let m = sample();
        assert_eq!(m.steps.len(), 2);
        assert_eq!(m.steps[0].file.as_deref(), Some("/boot/config/domain.cfg"));
        assert_eq!(m.steps[0].command, None);
        assert_eq!(
            m.steps[1].command.as_deref(),
            Some("/etc/rc.d/rc.libvirt restart")
        );
    }

    #[test]
    fn manual_round_trips_camel_case_and_tagged() {
        let r = Remediation::from(sample());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["type"], "manual");
        assert_eq!(v["reason"], "no API exists for this setting");
        assert_eq!(v["steps"][1]["command"], "/etc/rc.d/rc.libvirt restart");
        assert!(v["steps"][1].get("file").is_none());
        let back: Remediation = serde_json::from_value(v).unwrap();
        let Remediation::Manual(m) = back else {
            panic!("expected manual")
        };
        assert_eq!(m, sample());
    }

    #[test]
    fn automatic_round_trips() {
        let r = Remediation::Automatic {
            tool: "unraid.vm.enable".into(),
            args: serde_json::json!({"id": "x"}).into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["type"], "automatic");
        assert_eq!(v["tool"], "unraid.vm.enable");
        let back: Remediation = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Remediation::Automatic { tool, .. } if tool == "unraid.vm.enable"));
    }

    #[test]
    fn blocked_serializes_camel_case_and_converts_to_error() {
        let b = Blocked::new("unraid.vm_service_disabled", "VM service is off")
            .with_remediation(sample());
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["available"], false);
        assert_eq!(v["reasonCode"], "unraid.vm_service_disabled");
        assert_eq!(v["remediation"]["type"], "manual");

        let e = OrcaError::from(b);
        assert_eq!(e.kind, crate::ErrorKind::Unavailable);
        assert_eq!(e.code.as_deref(), Some("unraid.vm_service_disabled"));
        assert!(e.remediation.is_some());
    }

    #[test]
    fn manual_renders_numbered_steps() {
        let text = Remediation::from(sample()).to_string();
        assert!(text.starts_with("Manual steps required: no API exists"));
        assert!(text.contains("Warning: running VMs stop"));
        assert!(text.contains("  1. Enable VMs\n"));
        assert!(text.contains("     file:    /boot/config/domain.cfg\n"));
        assert!(text.contains("  2. Restart\n"));
        assert!(text.contains("     run:     /etc/rc.d/rc.libvirt restart\n"));
        assert!(text.ends_with("This unblocks: vm.list"));
    }
}
