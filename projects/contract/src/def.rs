//! `OrcaToolDef` — metadata trait. The `OrcaTool` supertrait (defined in
//! this same crate) anchors on it.
//!
//! Carries only types/consts — no `run` method, no async.

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Compile-time metadata for an OrcaTool — everything except `run`.
///
/// The `OrcaTool` trait requires this as a supertrait, so every tool's
/// NAME / DESCRIPTION / Args / Output live here exactly once.
pub trait OrcaToolDef: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// Whether this tool may be invoked by a paired system via `mesh/exec`.
    /// Default is **off** — opt in per-tool. Destructive or identity-tied ops
    /// (uninstall, dev_disable, key rotation) MUST stay false.
    const REMOTE_OK: bool = false;
    /// Minimum role required to invoke this tool via authenticated surfaces
    /// (REST, MCP-over-HTTP). `"any"` (default) means any authenticated identity
    /// passes; `"admin"` requires the caller's `AuthIdentity::role == "admin"`.
    ///
    /// Enforcement points: REST middleware on `/api/v1/*`, `mesh/exec` (which
    /// has no human identity and therefore refuses any admin-role tool), and
    /// `dispatch::ToolWrapper::run_json` for every call whose ctx carries a
    /// caller. A call with no caller (an in-process owner path) is not gated.
    const REQUIRED_ROLE: &'static str = "any";
    /// Whether this tool is a **data mutation** — a write against an external
    /// managed system (a proxmox VM create, an unraid plugin install, …) as
    /// opposed to a control-plane admin op (auth, secrets, system, config).
    ///
    /// Data mutations default to `REQUIRED_ROLE = "admin"`, but this flag lets a
    /// non-admin identity that has *opted in* (an API token / session granted
    /// the `can_mutate` capability) invoke them — see the dispatch role gate. It
    /// is deliberately narrow: control-plane admin tools leave this `false`, so
    /// the opt-in can never become a backdoor to them. Default **off**; the
    /// surface generators set it on generated mutating operations.
    const DATA_MUTATION: bool = false;
    /// Whether invoking this tool APPLIES changes and therefore honours the
    /// `dryRun` opt-in. **Execute is the default**: a gated tool called with
    /// `dryRun: true` describes what it would do
    /// (`contract::plan::ExecutionPlan`) and changes nothing. The role check
    /// runs first, so an admin verb's dry run is admin-only too.
    ///
    /// Enforced centrally in `dispatch::erased::ToolWrapper::run_json`, which
    /// every surface funnels through — so CLI, MCP, REST and mesh/exec all
    /// inherit it rather than each verb re-implementing a flag. The flag is
    /// read from the raw args before they are deserialized, so a gated tool's
    /// `Args` need not carry the field.
    ///
    /// Distinct from [`Self::DATA_MUTATION`], which asks *who may call this*.
    /// This asks *may it be previewed*. A tool is normally both.
    const EXECUTE_GATED: bool = false;
    /// Whether an ungated tool honours `dryRun` itself. When true,
    /// `dryRun: true` runs the body with `ToolCtx::dry_run()` set and the body
    /// must report what it would do without applying it. An ungated tool that
    /// is not aware refuses `dryRun` rather than ignoring it.
    const DRY_RUN_AWARE: bool = false;
    /// Whether this tool must run in the calling process (pre-daemon ops only:
    /// `install`, `daemon`, `system bootstrap`, `--version`). When false (the
    /// default), CLI + MCP-stdio invocations are proxied to the local daemon's
    /// HTTP endpoint so every surface runs the same handler. When true, the
    /// CLI runs the tool body in-process — required for tools that bring up
    /// or replace the daemon itself.
    const LOCAL_ONLY: bool = false;

    type Args: DeserializeOwned + Serialize + JsonSchema + Send;
    type Output: Serialize + DeserializeOwned + JsonSchema + Send + 'static;

    /// Whether [`Self::neutralize_for_dry_run`] is overridden. Set alongside it
    /// by `#[orca_tool(..., dry_run_neutralize = ...)]`. A hand impl must set
    /// it only together with a `neutralize_for_dry_run` override.
    const DRY_RUN_NEUTRALIZES: bool = false;

    /// Clear every apply opt-in on `args` before a dry run is forwarded to a
    /// peer. A peer that predates `dryRun` ignores it, so the forwarded args
    /// alone must make it probe rather than apply. Set via
    /// `#[orca_tool(..., dry_run_neutralize = path::to::fn)]`.
    fn neutralize_for_dry_run(_args: &mut Self::Args) {}
}

/// Surface-reorg metadata. `NAME` is `<DOMAIN>.<VERB>` by convention; the
/// extra split lets the unified CLI/REST router build `orca <DOMAIN> <VERB>`
/// subcommands and per-domain REST prefixes without re-parsing `NAME`.
///
/// Optional supertrait: only ops migrated to the unified surface need to
/// implement it. Existing tools that just satisfy `OrcaToolDef` keep working.
pub trait OrcaOp: OrcaToolDef {
    const DOMAIN: &'static str;
    const VERB: &'static str;
}
