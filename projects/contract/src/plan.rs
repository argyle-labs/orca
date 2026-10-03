//! The dry-run plan every execute-gated tool returns when invoked without an
//! explicit `execute` opt-in.
//!
//! Mutating verbs are dry-run by DEFAULT: calling one with no opt-in must
//! describe what it would change and change nothing. This is the shape that
//! description takes, so every surface (REST, MCP, CLI, peacock) renders one
//! thing rather than each verb inventing its own "would have" payload.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::json_any::JsonAny;

/// One thing a verb would do. `target` is the addressable thing being changed
/// (a host, a container, a datastore path), `action` what would happen to it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlannedChange {
    /// What would be changed — a host, unit, container, path. Addressable, so
    /// an operator can tell two otherwise-identical changes apart.
    pub target: String,
    /// What would happen to it: `create`, `delete`, `overwrite`, `restart`, …
    pub action: String,
    /// Optional specifics: old → new, sizes, counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PlannedChange {
    pub fn new(target: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            action: action.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// What a gated verb WOULD do. Returned in place of the tool's own output when
/// the caller did not opt in to `execute`.
///
/// `dryRun` is always `true` here — it exists so a caller that cannot tell the
/// two response shapes apart structurally still has one unambiguous field to
/// branch on. A response without it applied changes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPlan {
    /// Always `true`. Nothing was changed.
    pub dry_run: bool,
    /// Canonical `<domain>.<verb>` name of the tool that was planned.
    pub tool: String,
    /// One-line human summary of the intent.
    pub summary: String,
    /// The changes this verb would make. **Empty means "this verb has not
    /// implemented plan detail yet"**, NOT "nothing would change" — those are
    /// very different claims and a UI must not present the first as the second.
    /// `detailed` disambiguates them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<PlannedChange>,
    /// Whether `changes` is a real enumeration. False = the generic gate
    /// response; the verb refused nothing, it simply cannot yet describe itself.
    pub detailed: bool,
    /// The arguments as received, echoed so an operator can confirm the inputs
    /// before opting in, with every secret-shaped field redacted (the key is
    /// kept, the value replaced — see [`ExecutionPlan::generic`]). Free-form by
    /// nature — it is whatever this verb's Args are — which is what `JsonAny`
    /// is for.
    #[allow(clippy::disallowed_types)]
    pub inputs: JsonAny,
    /// How to actually apply it.
    pub how_to_execute: String,
}

impl ExecutionPlan {
    /// The generic plan used when a gated verb has not implemented its own.
    /// Deliberately honest: `detailed: false` and no invented changes.
    ///
    /// `inputs` is redacted here, because this is the one constructor every
    /// gated verb's dry-run funnels through. Echoing args verbatim printed
    /// `secrets.upsert.value`, `spec.update.token`, `model.create.apiKey` and
    /// friends straight to stdout and into anything that logged the plan; a
    /// per-verb fix would have leaked again on the next verb added.
    #[allow(clippy::disallowed_types)]
    pub fn generic(tool: &str, inputs: JsonAny) -> Self {
        Self {
            dry_run: true,
            tool: tool.to_string(),
            summary: format!("{tool} would run with the inputs below; nothing was changed"),
            changes: Vec::new(),
            detailed: false,
            inputs: utils::scrub::redact_json(inputs.0).into(),
            how_to_execute: format!("re-invoke {tool} with `execute: true` to apply"),
        }
    }

    /// Attach a real enumeration of changes, flipping `detailed` on.
    pub fn detailed(mut self, summary: impl Into<String>, changes: Vec<PlannedChange>) -> Self {
        self.summary = summary.into();
        self.changes = changes;
        self.detailed = true;
        self
    }
}

/// Field name of the universal execute opt-in, in wire (camelCase) form. One
/// spelling fleet-wide — `apply`, `force` and `reconcile_dry` are legacy
/// spellings of this same concept and migrate onto it.
pub const EXECUTE_FIELD: &str = "execute";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_plan_is_honest_about_having_no_detail() {
        let p = ExecutionPlan::generic("backup.run", JsonAny::default());
        assert!(p.dry_run);
        assert!(!p.detailed, "generic plan must not claim to be detailed");
        assert!(
            p.changes.is_empty(),
            "generic plan must not invent changes it cannot know"
        );
        assert!(p.how_to_execute.contains("execute"));
    }

    /// The measured defect: `printf 'CANARY-VALUE-12345' | orca secrets upsert
    /// … --value-stdin` printed the literal secret in the dry-run output.
    #[test]
    #[allow(clippy::disallowed_types)]
    fn plan_never_echoes_a_secret_but_keeps_the_key() {
        let inputs = serde_json::json!({
            "name": "canary.probe",
            "backend": "inline",
            "value": "CANARY-VALUE-12345",
        });
        let p = ExecutionPlan::generic("secrets.upsert", inputs.into());
        let text = serde_json::to_string(&p).expect("serialize plan");
        assert!(
            !text.contains("CANARY-VALUE-12345"),
            "secret echoed in plan: {text}"
        );
        // The key must survive — a dry-run that drops the field lies about
        // which inputs would be written.
        assert!(text.contains("\"value\""), "key was dropped: {text}");
        assert_eq!(p.inputs.0["value"], utils::scrub::REDACTED);
        assert_eq!(p.inputs.0["name"], "canary.probe");
        assert_eq!(p.inputs.0["backend"], "inline");
    }

    #[test]
    #[allow(clippy::disallowed_types)]
    fn plan_redacts_secrets_nested_in_objects_and_arrays() {
        let inputs = serde_json::json!({
            "host": "ct/107",
            "auth": { "apiKey": "CANARY-VALUE-12345" },
            "shares": [
                { "path": "/mnt/a", "password": "CANARY-VALUE-12345" },
                { "path": "/mnt/b", "nested": { "refresh_token": "CANARY-VALUE-12345" } },
            ],
        });
        let p = ExecutionPlan::generic("storage.share.create", inputs.into());
        let text = serde_json::to_string(&p).expect("serialize plan");
        assert!(
            !text.contains("CANARY-VALUE-12345"),
            "nested secret echoed in plan: {text}"
        );
        // Control: the addressing fields an operator confirms against are intact.
        assert_eq!(p.inputs.0["host"], "ct/107");
        assert_eq!(p.inputs.0["shares"][0]["path"], "/mnt/a");
        assert_eq!(p.inputs.0["shares"][1]["path"], "/mnt/b");
    }

    #[test]
    #[allow(clippy::disallowed_types)]
    fn redaction_survives_attaching_change_detail() {
        // `detailed()` must not reconstruct `inputs` from the raw args.
        let inputs = serde_json::json!({ "token": "CANARY-VALUE-12345" });
        let p = ExecutionPlan::generic("spec.update", inputs.into()).detailed(
            "would replace 1 spec",
            vec![PlannedChange::new("spec", "overwrite")],
        );
        let text = serde_json::to_string(&p).expect("serialize plan");
        assert!(!text.contains("CANARY-VALUE-12345"), "{text}");
    }

    #[test]
    fn detailed_plan_flips_the_flag_and_keeps_changes() {
        let p = ExecutionPlan::generic("backup.restore", JsonAny::default()).detailed(
            "would overwrite 2 paths",
            vec![
                PlannedChange::new("ct/107:/config", "overwrite").with_detail("4669 files"),
                PlannedChange::new("ct/107:/data", "overwrite"),
            ],
        );
        assert!(p.detailed);
        assert_eq!(p.changes.len(), 2);
        assert_eq!(p.changes[0].detail.as_deref(), Some("4669 files"));
        assert_eq!(p.summary, "would overwrite 2 paths");
    }

    #[test]
    fn plan_round_trips_and_keeps_dry_run_visible() {
        // dryRun must survive serialization: it is the one field a caller that
        // cannot distinguish response shapes structurally branches on.
        let p = ExecutionPlan::generic("storage.share.delete", JsonAny::default());
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"dryRun\":true"), "got {json}");
        let back: ExecutionPlan = serde_json::from_str(&json).unwrap();
        assert!(back.dry_run);
        assert!(!back.detailed);
    }
}
