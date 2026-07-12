//! FFI proxy seam for the `container_runtime` capability domain.
//!
//! Mirrors the `storage` / `service` domain proxies: a plugin cdylib advertises
//! a `container_runtime` backend, and the loader hands us an [`InvokeThunk`]
//! that maps an op to a `"{prefix}.{op}"` call across the FFI boundary. We wrap
//! that thunk in a [`ContainerRuntimeProxy`] implementing [`RuntimeAdapter`],
//! so a plugin-provided runtime adapter (docker/bollard, lxc/Proxmox-API) drives
//! the reconciler exactly like an in-process adapter. No concrete runtime client
//! (bollard, PVE API) lives in core — each rides in its owning plugin.
//!
//! Plugins reuse [`dispatch_op`] to route the op set back onto their own
//! `dyn RuntimeAdapter` without hand-writing the match, symmetric with
//! `storage::dispatch_op`.

use std::sync::Arc;

use derive::orca_async;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{
    AdapterError, Container, ExecOutput, HostObservation, ListFilter, Liveness, LogTail,
    RuntimeAdapter, RuntimeKind, WedgeRecoverer, register_entry,
};

/// The op→JSON thunk a proxy drives to reach the plugin. Identical shape to the
/// loader's `BackendInvoke` and the storage/service thunks, so it passes through
/// unwrapped: `(op, args_json) -> Result<result_json, error_json>`.
pub type InvokeThunk = Arc<dyn Fn(&str, String) -> Result<String, String> + Send + Sync + 'static>;

// ── Wire arg shapes (the designated FFI dispatch seam) ───────────────────────

#[derive(Serialize, Deserialize)]
struct IdArg {
    id: String,
}

#[derive(Serialize, Deserialize)]
struct LogsArg {
    id: String,
    tail: LogTail,
}

#[derive(Serialize, Deserialize)]
struct ExecArg {
    id: String,
    cmd: Vec<String>,
    stdin: Option<String>,
}

// ── Host-side proxy ──────────────────────────────────────────────────────────

/// A [`RuntimeAdapter`] backed by a plugin across the FFI boundary. Each async
/// method serializes its args, calls the sync `invoke` on a blocking thread
/// (like `StorageProxy`), and deserializes the result. `kind` is fixed at
/// registration; `wedge_capable` reflects the backend's advertised capability.
struct ContainerRuntimeProxy {
    kind: RuntimeKind,
    wedge_capable: bool,
    invoke: InvokeThunk,
}

impl ContainerRuntimeProxy {
    /// Serialize `args`, run the sync thunk off the async runtime, decode the
    /// result. FFI transport/decode failures and plugin-reported errors both
    /// map back into [`AdapterError`] — a plugin error is a JSON-encoded
    /// `AdapterError` (preserving the variant); anything else is `Transport`.
    async fn call<A: Serialize, R: DeserializeOwned>(
        &self,
        op: &'static str,
        args: A,
    ) -> Result<R, AdapterError> {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| AdapterError::Malformed(format!("encode {op} args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
            .await
            .map_err(|e| AdapterError::Transport(format!("{op} join: {e}")))?
            .map_err(|e| {
                serde_json::from_str::<AdapterError>(&e).unwrap_or(AdapterError::Transport(e))
            })?;
        serde_json::from_str(&out)
            .map_err(|e| AdapterError::Malformed(format!("decode {op} result: {e}")))
    }
}

#[orca_async]
impl RuntimeAdapter for ContainerRuntimeProxy {
    fn kind(&self) -> RuntimeKind {
        self.kind
    }

    async fn list(&self, filter: &ListFilter) -> Result<Vec<Container>, AdapterError> {
        self.call("list", filter).await
    }

    async fn inspect(&self, id: &str) -> Result<Container, AdapterError> {
        self.call("inspect", IdArg { id: id.to_string() }).await
    }

    async fn start(&self, id: &str) -> Result<(), AdapterError> {
        self.call("start", IdArg { id: id.to_string() }).await
    }

    async fn stop(&self, id: &str) -> Result<(), AdapterError> {
        self.call("stop", IdArg { id: id.to_string() }).await
    }

    async fn restart(&self, id: &str) -> Result<(), AdapterError> {
        self.call("restart", IdArg { id: id.to_string() }).await
    }

    async fn logs(&self, id: &str, tail: LogTail) -> Result<String, AdapterError> {
        self.call(
            "logs",
            LogsArg {
                id: id.to_string(),
                tail,
            },
        )
        .await
    }

    async fn exec(
        &self,
        id: &str,
        cmd: &[String],
        stdin: Option<String>,
    ) -> Result<ExecOutput, AdapterError> {
        self.call(
            "exec",
            ExecArg {
                id: id.to_string(),
                cmd: cmd.to_vec(),
                stdin,
            },
        )
        .await
    }

    async fn observe(&self, container: &Container) -> HostObservation {
        self.call("observe", container).await.unwrap_or_default()
    }

    async fn probe_liveness(&self, container: &Container) -> Liveness {
        self.call("probe_liveness", container)
            .await
            .unwrap_or(Liveness::Unknown)
    }

    fn wedge_recoverer(&self) -> Option<&dyn WedgeRecoverer> {
        // The proxy itself performs recovery over FFI, but only when the backend
        // advertised the capability — otherwise the reconciler escalates rather
        // than round-tripping an op the plugin doesn't implement.
        self.wedge_capable.then_some(self as &dyn WedgeRecoverer)
    }
}

#[orca_async]
impl WedgeRecoverer for ContainerRuntimeProxy {
    async fn attempt_unwedge(&self, container: &Container) -> Result<(), AdapterError> {
        self.call("attempt_unwedge", container).await
    }
}

/// Capability string a `container_runtime` backend sets in its `BackendDef` to
/// declare it can attempt in-place wedge recovery.
pub const CAP_WEDGE_RECOVER: &str = "wedge_recover";

/// Register a plugin-backed runtime adapter from a loaded backend descriptor.
/// Mirrors `storage::register_from_def` — the loader calls this for a
/// `domain = "container_runtime"` backend. `kind` is the [`RuntimeKind`] string
/// (`docker`/`lxc`/…); `capabilities` may include [`CAP_WEDGE_RECOVER`].
pub fn register_from_def(
    _name: String,
    kind: &str,
    capabilities: &[String],
    invoke: InvokeThunk,
) -> Result<(), String> {
    let kind = RuntimeKind::from_str(kind)
        .ok_or_else(|| format!("unknown container runtime kind '{kind}'"))?;
    let wedge_capable = capabilities.iter().any(|c| c == CAP_WEDGE_RECOVER);
    register_entry(
        Some(_name),
        Arc::new(ContainerRuntimeProxy {
            kind,
            wedge_capable,
            invoke,
        }),
    );
    Ok(())
}

// ── Plugin-side dispatch ──────────────────────────────────────────────────────

/// Route a proxied `op` to a plugin's own `dyn RuntimeAdapter` and encode the
/// result for the FFI boundary. Symmetric with [`ContainerRuntimeProxy`] — a
/// plugin's `backend_dispatch` delegates here so it never hand-writes the op
/// match. Errors are JSON-encoded [`AdapterError`]s so the host proxy can
/// reconstruct the variant.
pub async fn dispatch_op(
    adapter: &dyn RuntimeAdapter,
    op: &str,
    args_json: &str,
) -> Result<String, String> {
    fn dec<T: DeserializeOwned>(op: &str, args_json: &str) -> Result<T, String> {
        serde_json::from_str(args_json)
            .map_err(|e| enc_err(&AdapterError::Malformed(format!("decode {op} args: {e}"))))
    }
    fn enc<T: Serialize>(v: &T) -> Result<String, String> {
        serde_json::to_string(v).map_err(|e| format!("encode result: {e}"))
    }
    fn enc_err(e: &AdapterError) -> String {
        serde_json::to_string(e).unwrap_or_else(|_| e.to_string())
    }

    match op {
        "list" => {
            let filter: ListFilter = dec(op, args_json)?;
            adapter
                .list(&filter)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "inspect" => {
            let a: IdArg = dec(op, args_json)?;
            adapter
                .inspect(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "start" => {
            let a: IdArg = dec(op, args_json)?;
            adapter
                .start(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|()| enc(&()))
        }
        "stop" => {
            let a: IdArg = dec(op, args_json)?;
            adapter
                .stop(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|()| enc(&()))
        }
        "restart" => {
            let a: IdArg = dec(op, args_json)?;
            adapter
                .restart(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|()| enc(&()))
        }
        "logs" => {
            let a: LogsArg = dec(op, args_json)?;
            adapter
                .logs(&a.id, a.tail)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "exec" => {
            let a: ExecArg = dec(op, args_json)?;
            adapter
                .exec(&a.id, &a.cmd, a.stdin)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "observe" => {
            let c: Container = dec(op, args_json)?;
            enc(&adapter.observe(&c).await)
        }
        "probe_liveness" => {
            let c: Container = dec(op, args_json)?;
            enc(&adapter.probe_liveness(&c).await)
        }
        "attempt_unwedge" => {
            let c: Container = dec(op, args_json)?;
            match adapter.wedge_recoverer() {
                Some(r) => r
                    .attempt_unwedge(&c)
                    .await
                    .map_err(|e| enc_err(&e))
                    .and_then(|()| enc(&())),
                None => Err(enc_err(&AdapterError::Refused(
                    "wedge recovery not supported by this runtime adapter".into(),
                ))),
            }
        }
        other => Err(enc_err(&AdapterError::Refused(format!(
            "unknown container_runtime op: {other}"
        )))),
    }
}
