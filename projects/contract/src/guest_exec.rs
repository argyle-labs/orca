//! Cross-crate guest-exec capability: run a command inside a guest and capture
//! its stdout/stderr/exit code, plus write a file into a guest.
//!
//! This is the *same universal provider pattern* as [`crate::ups`] /
//! [`crate::diagnostics`]: core owns the primitives, the types, and the
//! orchestration *policy*; a plugin implements the mechanics as a provider. A
//! provider is a channel into guests — the proxmox plugin backs it with the QEMU
//! guest agent (VM path); a later docker/ssh backend fits the *same* trait
//! unchanged. Nothing here is PVE-specific: [`GuestRef`] locates a guest in the
//! provider's own namespace, and [`ExecRequest`] carries the run parameters.
//!
//! ## Blocking-from-the-caller contract, provider-run loop
//!
//! [`GuestExec::exec`] is blocking from the caller's view: it returns the final
//! [`ExecOutput`] with exit code + captured output. Some transports (the QEMU
//! guest agent) are *asynchronous* under the hood — a start call returns a pid
//! and the caller must poll a status endpoint until the process exits. That
//! pid+poll loop is a transport detail the provider owns; it is deliberately not
//! in the trait, because a synchronous transport (ssh, `docker exec`) has no pid
//! to poll and exposing one would leak the guest-agent model into a generic
//! contract. What core *does* own is the loop **policy** — the poll interval, the
//! overall deadline, and the output cap — carried in [`ExecRequest`] and defaulted
//! from the `DEFAULT_*` constants here. The provider runs the mechanical loop
//! honouring that policy; the decision of *how long / how often / how much* is
//! core's.
//!
//! ## Secret safety (honoured now; provisioning lands in a later slice)
//!
//! Anything passed in [`ExecRequest::command`] becomes argv inside the guest — it
//! lands in the guest's `/proc/<pid>/cmdline`, the guest-agent logs, and the PVE
//! task log. **Never put a secret in argv.** The sanctioned channel for sensitive
//! input is [`ExecRequest::input_data`] (stdin) or a [`GuestExec::write_file`]
//! into a mode-restricted path. To keep secrets out of tracing/error output,
//! [`ExecRequest`] and [`WriteFileRequest`] hand-roll `Debug` to redact the
//! payload bytes. A future secret-provisioning slice injects secrets *only* over
//! these two channels.
//!
//! ## Provider registry
//!
//! A provider registers into a process-global registry — either in-process or,
//! for an external subprocess plugin, via the [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "guest_exec"`.

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`
// (the wire already produces a parsed value; no String hop). This module names
// that type in its `InvokeThunk`/proxy — the sanctioned opaque seam, scoped here.
#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

// ── Orchestration policy (core-owned) ────────────────────────────────────────

/// Default overall deadline for a guest exec, in **milliseconds**. Applied by a
/// provider when [`ExecRequest::timeout_ms`] is `None`. Past it, the provider
/// stops polling and returns [`ExecOutput::timed_out`] `= true`.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Default interval between status polls, in **milliseconds**. Applied when
/// [`ExecRequest::poll_interval_ms`] is `None`.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 500;

/// Default cap on captured stdout/stderr, in **bytes**, each. Applied when
/// [`ExecRequest::max_output_bytes`] is `None`. Output beyond it is dropped and
/// the matching `*_truncated` flag is set.
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1_048_576;

// ── Model ─────────────────────────────────────────────────────────────────

/// Locates one guest in a provider's own namespace. Kept transport-agnostic: the
/// proxmox VM backend reads `scope` as the PVE endpoint, `node` as the PVE node,
/// and `id` as the vmid; a docker backend would read `scope` as the docker host
/// and `id` as the container; an ssh backend reads `id` as the host. The provider
/// interprets the fields — core never parses them.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct GuestRef {
    /// Provider-scoped locator of the host/cluster the guest lives in (a PVE
    /// endpoint name, a docker host). `None` for a single-target provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Node within the scope (a PVE node). Providers that don't nest ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The guest's provider-native id (a PVE vmid, a container id/name, a host).
    pub id: String,
}

/// A command to run inside a guest, plus the loop policy the provider honours.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct ExecRequest {
    /// Program + arguments. **Never a secret** — argv is world-visible inside the
    /// guest (`/proc`) and in the guest-agent + PVE task logs. Pass sensitive
    /// input over [`input_data`](ExecRequest::input_data) instead.
    pub command: Vec<String>,
    /// Bytes fed to the process's stdin. The sanctioned channel for sensitive
    /// input. Redacted from this type's `Debug`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_data: Option<Vec<u8>>,
    /// Overall deadline in **milliseconds**. `None` = [`DEFAULT_TIMEOUT_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Interval between status polls in **milliseconds**. `None` =
    /// [`DEFAULT_POLL_INTERVAL_MS`]. Ignored by a synchronous transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
    /// Per-stream cap on captured output in **bytes**. `None` =
    /// [`DEFAULT_MAX_OUTPUT_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

impl ExecRequest {
    /// The effective timeout: the request's value or [`DEFAULT_TIMEOUT_MS`].
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }
    /// The effective poll interval: the request's value or
    /// [`DEFAULT_POLL_INTERVAL_MS`].
    pub fn poll_interval_ms(&self) -> u64 {
        self.poll_interval_ms.unwrap_or(DEFAULT_POLL_INTERVAL_MS)
    }
    /// The effective per-stream output cap: the request's value or
    /// [`DEFAULT_MAX_OUTPUT_BYTES`].
    pub fn max_output_bytes(&self) -> u64 {
        self.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
    }
}

// Redact stdin bytes so a secret passed over the sanctioned channel never lands
// in a tracing line or an error rendered with `{:?}`.
impl std::fmt::Debug for ExecRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecRequest")
            .field("command", &self.command)
            .field(
                "input_data",
                &self
                    .input_data
                    .as_ref()
                    .map(|d| format!("<{} bytes>", d.len())),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field("poll_interval_ms", &self.poll_interval_ms)
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

/// The result of a [`GuestExec::exec`] — blocking from the caller's view.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct ExecOutput {
    /// Process exit code, when it terminated normally. `None` if it was killed by
    /// a signal (see [`signal`](ExecOutput::signal)) or timed out before exiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Signal/exception number, when the process was abnormally terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i64>,
    /// Captured stdout (UTF-8 lossy), up to the request's output cap.
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy), up to the request's output cap.
    pub stderr: String,
    /// The deadline elapsed before the process exited; output is whatever was
    /// captured up to that point and `exit_code` is `None`.
    #[serde(default)]
    pub timed_out: bool,
    /// stdout was truncated (by the guest agent or by the output cap).
    #[serde(default)]
    pub stdout_truncated: bool,
    /// stderr was truncated (by the guest agent or by the output cap).
    #[serde(default)]
    pub stderr_truncated: bool,
}

/// A file to write into a guest, with optional POSIX mode/owner. NOTE: not every
/// transport sets mode/owner natively (the QEMU guest agent's `file-write` writes
/// content only), so a provider that lacks native support applies them with a
/// chained `chmod`/`chown` exec after the write. When `mode`/`owner` are `None`
/// the guest's defaults (the agent's effective umask/uid) apply.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct WriteFileRequest {
    /// Absolute path inside the guest.
    pub path: String,
    /// File contents. May carry a secret — redacted from this type's `Debug`.
    pub contents: Vec<u8>,
    /// POSIX mode as an octal string (e.g. `"0640"`). Applied via a chained
    /// `chmod` when the transport can't set it at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Owner as `user` or `user:group`. Applied via a chained `chown` when the
    /// transport can't set it at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl std::fmt::Debug for WriteFileRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteFileRequest")
            .field("path", &self.path)
            .field("contents", &format!("<{} bytes>", self.contents.len()))
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .finish()
    }
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A guest-exec channel — one per provider (`proxmox`, later `docker`/`ssh`).
/// Registered into the process-global registry so a consumer routes to it by
/// name without naming a concrete transport.
pub trait GuestExec: Send + Sync {
    /// Provider/registry name (e.g. `"proxmox"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    /// Run `req.command` inside `guest`, returning once it exits (or the deadline
    /// elapses). Blocking from the caller's view — the provider owns any pid+poll
    /// loop internally, honouring the request's timeout/interval/output-cap policy.
    fn exec(&self, guest: GuestRef, req: ExecRequest) -> BoxFuture<'_, Result<ExecOutput>>;

    /// Write a file into `guest`, applying `mode`/`owner` when set (natively or via
    /// a chained `chmod`/`chown`, per the transport).
    fn write_file(&self, guest: GuestRef, req: WriteFileRequest) -> BoxFuture<'_, Result<()>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn GuestExec>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a guest-exec provider. Re-registering the same `name()` replaces the
/// existing entry so a dev rebuild / plugin reload doesn't duplicate it.
pub fn register_provider(provider: Arc<dyn GuestExec>) {
    let mut g = GLOBAL.write().expect("guest_exec registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn GuestExec>> {
    GLOBAL.read().expect("guest_exec registry poisoned").clone()
}

/// Look up a single provider by registry name.
pub fn provider(name: &str) -> Option<Arc<dyn GuestExec>> {
    GLOBAL
        .read()
        .expect("guest_exec registry poisoned")
        .iter()
        .find(|p| p.name() == name)
        .cloned()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("guest_exec registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── Surface entry points ─────────────────────────────────────────────────────

/// Route an `exec` to the named provider.
pub async fn exec(provider_name: &str, guest: GuestRef, req: ExecRequest) -> Result<ExecOutput> {
    let p = provider(provider_name)
        .ok_or_else(|| anyhow::anyhow!("no guest_exec provider named '{provider_name}'"))?;
    p.exec(guest, req).await
}

/// Route a `write_file` to the named provider.
pub async fn write_file(provider_name: &str, guest: GuestRef, req: WriteFileRequest) -> Result<()> {
    let p = provider(provider_name)
        .ok_or_else(|| anyhow::anyhow!("no guest_exec provider named '{provider_name}'"))?;
    p.write_file(guest, req).await
}

// ── Wire args ───────────────────────────────────────────────────────────────
// Typed args objects each proxied op serializes across the FFI invoke boundary.
// Defined (not `json!`'d) so both halves — the host `GuestExecProxy` (encode) and
// the plugin-side `dispatch_op` (decode) — share one shape.

/// Args for the `exec` op.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ExecArgs {
    pub guest: GuestRef,
    pub request: ExecRequest,
}

/// Args for the `write_file` op.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct WriteFileArgs {
    pub guest: GuestRef,
    pub request: WriteFileRequest,
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────────

/// The synchronous invoke thunk a loaded plugin's provider is driven through:
/// `(op, args) -> Result<result, error>`, all as `serde_json::Value`. Plain `Fn`
/// of `Value`s so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(&str, serde_json::Value) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

/// Operation names the [`GuestExecProxy`] invokes across the FFI boundary. The
/// plugin exposes tools `"{invoke_prefix}.{EXEC_OP|WRITE_FILE_OP}"`.
pub const EXEC_OP: &str = "exec";
pub const WRITE_FILE_OP: &str = "write_file";

/// Plugin-side dispatch: answer a proxied guest-exec op by calling the typed
/// [`GuestExec`]. `exec` decodes [`ExecArgs`]; `write_file` decodes
/// [`WriteFileArgs`]. Symmetric with the host-side proxy.
pub async fn dispatch_op(
    provider: &dyn GuestExec,
    op: &str,
    args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    fn err(msg: impl Into<String>) -> serde_json::Value {
        serde_json::Value::String(msg.into())
    }
    fn dec<T: serde::de::DeserializeOwned>(
        op: &str,
        args: serde_json::Value,
    ) -> std::result::Result<T, serde_json::Value> {
        serde_json::from_value(args).map_err(|e| err(format!("decode {op} args: {e}")))
    }
    match op {
        EXEC_OP => {
            let a: ExecArgs = dec(op, args)?;
            let out = provider
                .exec(a.guest, a.request)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        WRITE_FILE_OP => {
            let a: WriteFileArgs = dec(op, args)?;
            provider
                .write_file(a.guest, a.request)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            Ok(serde_json::Value::Null)
        }
        other => Err(err(format!("unknown guest_exec op: {other}"))),
    }
}

/// Build and register a [`GuestExec`] from a plugin backend descriptor plus an
/// [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch table
/// for `domain = "guest_exec"`.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(GuestExecProxy { name, invoke }));
    Ok(())
}

/// A [`GuestExec`] backed by a subprocess plugin reached over the JSON-proxy FFI
/// boundary. Each op offloads the synchronous [`InvokeThunk`] onto
/// `spawn_blocking` and (de)serializes JSON at the seam.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct GuestExecProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl GuestExecProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args))
                .await
                .map_err(|e| anyhow::anyhow!("guest_exec '{name}' {op} task panicked: {e}"))?
                .map_err(|e| {
                    anyhow::anyhow!(
                        "guest_exec '{name}' {op} failed: {}",
                        crate::render_invoke_error(&e)
                    )
                })?;
            serde_json::from_value(out)
                .map_err(|e| anyhow::anyhow!("guest_exec '{name}' {op} returned invalid JSON: {e}"))
        })
    }
}

#[cfg(feature = "in-process")]
impl GuestExec for GuestExecProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn exec(&self, guest: GuestRef, req: ExecRequest) -> BoxFuture<'_, Result<ExecOutput>> {
        let args = serde_json::to_value(ExecArgs {
            guest,
            request: req,
        })
        .unwrap_or_else(|_| serde_json::json!({}));
        self.call(EXEC_OP, args)
    }

    fn write_file(&self, guest: GuestRef, req: WriteFileRequest) -> BoxFuture<'_, Result<()>> {
        let args = serde_json::to_value(WriteFileArgs {
            guest,
            request: req,
        })
        .unwrap_or_else(|_| serde_json::json!({}));
        // `write_file` returns `Null`; decode into `()`.
        self.call(WRITE_FILE_OP, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeGuest {
        name: String,
    }

    impl GuestExec for FakeGuest {
        fn name(&self) -> &str {
            &self.name
        }
        fn exec(&self, guest: GuestRef, req: ExecRequest) -> BoxFuture<'_, Result<ExecOutput>> {
            Box::pin(async move {
                Ok(ExecOutput {
                    exit_code: Some(0),
                    signal: None,
                    stdout: format!("ran {:?} on {}", req.command, guest.id),
                    stderr: String::new(),
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })
            })
        }
        fn write_file(
            &self,
            _guest: GuestRef,
            _req: WriteFileRequest,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn request_defaults_resolve_from_constants() {
        let r = ExecRequest {
            command: vec!["echo".into(), "hi".into()],
            ..Default::default()
        };
        assert_eq!(r.timeout_ms(), DEFAULT_TIMEOUT_MS);
        assert_eq!(r.poll_interval_ms(), DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(r.max_output_bytes(), DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn debug_redacts_stdin_and_file_contents() {
        let r = ExecRequest {
            command: vec!["cat".into()],
            input_data: Some(b"super-secret-token".to_vec()),
            ..Default::default()
        };
        let dbg = format!("{r:?}");
        assert!(
            !dbg.contains("super-secret-token"),
            "stdin must be redacted"
        );
        assert!(dbg.contains("<18 bytes>"));

        let w = WriteFileRequest {
            path: "/etc/x".into(),
            contents: b"password=hunter2".to_vec(),
            mode: Some("0600".into()),
            owner: None,
        };
        let dbg = format!("{w:?}");
        assert!(!dbg.contains("hunter2"), "file contents must be redacted");
        assert!(dbg.contains("<16 bytes>"));
    }

    #[tokio::test]
    async fn register_route_and_deregister() {
        register_provider(Arc::new(FakeGuest {
            name: "guest-test".into(),
        }));
        let out = exec(
            "guest-test",
            GuestRef {
                id: "104".into(),
                node: Some("n1".into()),
                scope: Some("pve".into()),
            },
            ExecRequest {
                command: vec!["uname".into(), "-a".into()],
                ..Default::default()
            },
        )
        .await
        .expect("routes to provider");
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("104"));

        // Unknown provider is a clean error, never a panic.
        assert!(
            exec("nope", GuestRef::default(), ExecRequest::default())
                .await
                .is_err()
        );
        assert!(deregister_provider("guest-test"));
    }

    #[tokio::test]
    async fn dispatch_op_routes_exec_and_write_file() {
        let g = FakeGuest {
            name: "disp".into(),
        };
        let out = dispatch_op(
            &g,
            EXEC_OP,
            serde_json::json!({
                "guest": {"id": "200"},
                "request": {"command": ["true"]}
            }),
        )
        .await
        .expect("exec dispatches");
        assert_eq!(out["exit_code"], serde_json::json!(0));

        let w = dispatch_op(
            &g,
            WRITE_FILE_OP,
            serde_json::json!({
                "guest": {"id": "200"},
                "request": {"path": "/tmp/x", "contents": [104, 105]}
            }),
        )
        .await
        .expect("write_file dispatches");
        assert_eq!(w, serde_json::Value::Null);

        assert!(
            dispatch_op(&g, "teleport", serde_json::json!({}))
                .await
                .is_err()
        );
    }
}
