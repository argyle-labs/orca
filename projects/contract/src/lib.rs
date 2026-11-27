//! `orca-contract` — contract shared across every OrcaTool surface.
//!
//! Exposes the metadata traits (`OrcaToolDef`, `OrcaOp`), `OrcaError` /
//! `ErrorKind` / `OrcaResult`, `JsonAny`, the LLM protocol types
//! (`ToolCall`, `ToolDef`, `ToolResult`), and the runtime trait anchors —
//! `OrcaTool`, `ToolCtx`, `RemoteExec`.
//!
//! No inventory, no axum, no tokio — those live in `orca-dispatch`.

pub mod config;

/// Canonical hand-desugared async return for capability traits (no `async_trait`
/// macro, per the workspace rule). One definition here; every domain crate
/// (`unit`, `service`, …) re-uses it instead of redeclaring the alias.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Render an erased-invoke `Err(serde_json::Value)` into a human string for
/// proxy/host error messages. The erased-invoke boundary (the per-domain
/// `InvokeThunk`s and the loader's `BackendInvoke`) carries both args and
/// errors as `serde_json::Value` — the wire already produces a parsed value, so
/// there is no String hop. A `Value::String` is used verbatim (no surrounding
/// quotes); any other shape is JSON-rendered. This is the one place that
/// flattens an erased error `Value` back to text.
///
/// Plugin errors can echo upstream response bodies, so the rendered text is
/// secret-scrubbed before it reaches any caller.
#[allow(clippy::disallowed_types)]
pub fn render_invoke_error(err: &serde_json::Value) -> String {
    let text = match err {
        serde_json::Value::String(s) => s.clone(),
        other => utils::scrub::redact_json(other.clone()).to_string(),
    };
    #[cfg(feature = "in-process")]
    let text = utils::scrub::redact_text(&text);
    text
}

mod def;
pub use def::{OrcaOp, OrcaToolDef};

mod error;
pub use error::{ErrorKind, OrcaError, OrcaResult};

pub mod json_any;

/// Shared pagination primitive for every `*.list` verb (PageParams + Page<T>).
pub mod paging;
/// Structured way out of a blocked state (automatic op or manual steps).
pub mod remediation;
pub use remediation::{Blocked, ManualStep, ManualSteps, Remediation};
/// Dry-run plan returned by execute-gated tools (see `plan::ExecutionPlan`).
pub mod plan;
/// Per-verb risk level.
pub mod risk;
pub use risk::Risk;
/// Approval requests and their state machine.
pub mod approval;
/// Step-up policy and providers.
pub mod step_up;
// The re-export itself triggers the disallowed-type lint workspace-wide;
// defining + exposing the type is exactly what this crate exists to do.
#[allow(clippy::disallowed_types)]
pub use json_any::JsonAny;

mod types;
pub use types::{ToolCall, ToolDef, ToolResult};

#[cfg(all(test, feature = "in-process"))]
mod render_invoke_error_tests {
    use super::render_invoke_error;

    #[test]
    fn provider_error_tokens_reach_caller_redacted() {
        let err = serde_json::Value::String(
            r#"upstream 401: {"token":"abc123def456","ok":false} at https://api.example/v1?apikey=zz9988&x=1 Authorization: Bearer eyJhbGciOi.eyJzdWIi.c2ln"#
                .to_string(),
        );
        let out = render_invoke_error(&err);
        for leak in ["abc123def456", "zz9988", "eyJhbGciOi"] {
            assert!(!out.contains(leak), "{out}");
        }
        assert!(out.contains("upstream 401"), "{out}");
    }

    #[test]
    fn structured_provider_error_is_redacted() {
        let err = serde_json::json!({"message": "denied", "token": "abc123def456"});
        let out = render_invoke_error(&err);
        assert!(!out.contains("abc123def456"), "{out}");
        assert!(out.contains("denied"), "{out}");
    }
}

pub mod arg_hint;
pub use arg_hint::ArgRefusalHint;

pub mod cluster_roster;
mod ctx;
/// Ask every system the same question and merge the answers. `in-process`
/// only: it spawns, and a thin plugin links no tokio.
#[cfg(feature = "in-process")]
pub mod fanout;
pub mod mesh_relay;
pub use utils::invocation_scope;
pub mod owner;
mod remote;
pub mod roles;
mod tool;

pub use cluster_roster::{AggregateClusterRoster, ClusterEntry, ClusterNode, ClusterRoster};
pub use ctx::ToolCtx;
pub use remote::{CallerIdentity, PeerRef, RemoteExec};
pub use tool::OrcaTool;

pub mod topology;
pub use topology::TopologyClaim;

pub mod host_facts;
pub use host_facts::{HostFacts, HostFactsProvider};

pub mod secrets_backend;
pub use secrets_backend::SecretsBackend;

pub mod service_identity;
pub use service_identity::{ServicePrimitive, ServiceRegistration};

pub mod diagnostics;
pub use diagnostics::{DelegatedRepair, Finding, RepairOutcome, RepairSpec, Severity};

pub mod guest_exec;
pub use guest_exec::{ExecOutput, ExecRequest, GuestExec, GuestRef, WriteFileRequest};

pub mod health;

pub mod notification_source;

pub mod permissions;
pub use permissions::{PermCandidate, PermInfo, PermissionsProvider};

pub mod template_kind;
pub use template_kind::{
    ApplyOutcome, ApplyRequest, CredentialSlotRef, DestroyRequest, PlanStep, RenderRequest,
    RenderedPayload, SecretRef, StatusOutcome, StatusRequest, TargetDescriptor, TemplateKindDecl,
    TemplateKindProvider, TemplateLayer, TemplateRef,
};

pub mod security;

pub mod tunnel;
pub mod vpn;

pub mod ups;
pub use ups::{UpsConfig, UpsConfigOutcome, UpsProvider, UpsState};

pub mod web;
pub use web::{WebProvider, WebRequest, WebResponse, WebRoute};

pub mod subprocess_env;

pub mod plugin;
pub use subprocess_env::{EnvProvider, EnvVar};

pub mod plugin_instrument;

pub mod backup;
pub use backup::{
    BackupGate, BackupPolicy, BackupRef, BackupSchedule, BackupSpec, BackupStrategy,
    BackupTargetBinding, BackupTargetRef, ONE_GIB, Placement, PolicySource, Resolved,
    RestorePayload, Retention, resolve_retention, resolve_schedule,
};

pub mod guard;
pub use guard::{GuardViolation, UnitFacts, UnitGuard, partition_violations};

pub mod sync;
pub use sync::{
    SyncProvider, SyncProviderError, SyncReconcileArgs, SyncReconcileOutcome, SyncStatusArgs,
    SyncStatusOutcome, SyncTargetError, SyncTargetHealth, SyncTargetSpec,
};

pub mod unit;
