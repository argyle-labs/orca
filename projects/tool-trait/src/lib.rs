//! `OrcaToolDef` — wasm-safe metadata trait shared by orca-utils (native
//! `OrcaTool` supertrait) and orca-tools-def (wasm client codegen).
//!
//! Carries only types/consts — no `run` method, no async, no native deps.

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Compile-time metadata for an OrcaTool — everything except `run`.
///
/// Implementations of this trait are wasm-safe by construction. The native
/// `OrcaTool` trait in `orca-utils` requires this as a supertrait, so every
/// tool's NAME / DESCRIPTION / Args / Output live here exactly once.
pub trait OrcaToolDef: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// Whether this tool may be invoked by a paired pod peer via `pod/exec`.
    /// Default is **off** — opt in per-tool. Destructive or identity-tied ops
    /// (uninstall, dev_disable, key rotation) MUST stay false.
    const REMOTE_OK: bool = false;

    type Args: DeserializeOwned + Serialize + JsonSchema + Send;
    type Output: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
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
