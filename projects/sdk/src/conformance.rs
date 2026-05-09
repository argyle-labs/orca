//! SDK conformance suite — the plugin contract, executable.
//!
//! Every language SDK port (Go, Kotlin, TypeScript) targets the same
//! [`SCENARIO`]: connect, hello, declare a type, publish a value. A port is
//! "conformant" when its hello-world plugin, run against [`run_subprocess`],
//! returns a [`Report`] with all steps `Pass`.
//!
//! ## Why this lives in the SDK
//!
//! The conformance scenario IS part of the contract — same as the protocol
//! method shapes and the manifest format. Putting it in `projects/sdk` means:
//!
//! - One source of truth. Every port reads the canonical scenario the same way.
//! - The conformance runner uses the SDK's own transport, framing, jsonrpc,
//!   and pki — exercising the public surface a port has to reproduce.
//! - No dependency on `projects/server`. The runner embeds a minimal host
//!   that mirrors the production plugin host's wire behavior for the subset
//!   of methods the scenario exercises.
//!
//! ## What a conformant plugin must do
//!
//! When run as a subprocess, the plugin reads these env vars:
//!
//! | Var                 | Meaning                                              |
//! |---------------------|------------------------------------------------------|
//! | `ORCA_PLUGIN_ADDR`  | `host:port` of the conformance host (TCP+mTLS)       |
//! | `ORCA_PKI_DIR`      | Directory containing CA + this plugin's cert/key     |
//! | `ORCA_PLUGIN_ID`    | The plugin id to claim in `orca/hello` (= cert CN)   |
//! | `ORCA_MANIFEST_PATH`| Path to a canonical `orca-plugin.toml` to parse      |
//!
//! It then performs, in order:
//!
//! 1. **Parse manifest** — load `ORCA_MANIFEST_PATH` via the SDK's manifest
//!    parser. Round-trip success is observed indirectly via step 4.
//! 2. **Hello** — open mTLS to `ORCA_PLUGIN_ADDR`, send `orca/hello` claiming
//!    `plugin_id = ORCA_PLUGIN_ID`. Expect `ok=true`, `status="full"`.
//! 3. **Declare type** — send `orca/types.declare` with exactly one type whose
//!    `type_name` equals [`SCENARIO.type_decl.type_name`].
//! 4. **Publish value** — send `orca/context.publish` to
//!    [`SCENARIO.publish.context_id`] with a [`TypedValue`] whose
//!    `type_id == "<plugin_id>.<type_name>"` and a payload that round-trips
//!    `manifest.plugin.id` (so we can tell the manifest was actually parsed).
//! 5. **Exit cleanly** — terminate the process with status 0.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::framing::{read_frame, write_frame};
use crate::jsonrpc::{ErrorObject, Message, Request, Response};
use crate::pki;
use crate::tools::{
    TOOLS_CALL_METHOD, TOOLS_DECLARE_METHOD, ToolCallParams, ToolDeclaration, ToolsDeclareParams,
    ToolsDeclareResult,
};
use crate::transport::{
    ContextPublishParams, HelloParams, HelloResult, TypedValue, TypesDeclareParams,
    TypesDeclareResult,
};

// ── Scenario specification ────────────────────────────────────────────────────

/// What a conformant plugin must do, declaratively. Static so language ports
/// can copy values verbatim from the source.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub plugin_id: &'static str,
    pub type_name: &'static str,
    pub type_schema_version: &'static str,
    /// JSON Schema (as a JSON-encoded string) the declared type must use.
    pub type_schema_json: &'static str,
    pub context_id: &'static str,
    /// Required JSON pointer path in the published payload that must contain
    /// the plugin id (so we can tell the plugin actually parsed the manifest).
    pub manifest_id_payload_key: &'static str,
    /// Bare tool name the plugin must register. The host calls it with
    /// [`Self::tool_call_argument_value`] and expects the result payload
    /// to contain that same value at [`Self::tool_result_echo_key`] —
    /// proves the plugin's `tools/call` dispatch round-trips arguments.
    pub tool_name: &'static str,
    pub tool_input_schema_json: &'static str,
    pub tool_call_argument_key: &'static str,
    pub tool_call_argument_value: &'static str,
    pub tool_result_echo_key: &'static str,
}

/// The single canonical conformance scenario. Every port reproduces this.
pub const SCENARIO: Scenario = Scenario {
    plugin_id: "conformance-plug",
    type_name: "Greeting",
    type_schema_version: "0.1.0",
    type_schema_json: r#"{"type":"object","properties":{"text":{"type":"string"},"manifest_id":{"type":"string"}},"required":["text","manifest_id"]}"#,
    context_id: "conformance:hello",
    manifest_id_payload_key: "manifest_id",
    tool_name: "echo",
    tool_input_schema_json: r#"{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}"#,
    tool_call_argument_key: "value",
    tool_call_argument_value: "ping",
    tool_result_echo_key: "echoed",
};

/// Canonical manifest written to disk before the plugin starts. The plugin
/// reads it via `ORCA_MANIFEST_PATH`, parses it with the SDK manifest parser,
/// and echoes `manifest.plugin.id` back through the published payload.
pub const SCENARIO_MANIFEST: &str = r#"
[plugin]
id               = "conformance-plug"
version          = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./conformance-plug"
mode   = "process"
eager  = false

[surfaces]
mcp = true

[[capabilities]]
name        = "context.publish"
sensitivity = "general"
"#;

// ── Report types ──────────────────────────────────────────────────────────────

/// One observation step the runner checks. Steps are recorded in the order
/// the plugin must perform them; missing steps appear as `Fail` with detail.
#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    pub name: &'static str,
    pub status: StepStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pass,
    Fail,
}

/// Outcome of a conformance run. `passed` is true iff every step is `Pass`.
#[derive(Debug, Clone)]
pub struct Report {
    pub passed: bool,
    pub steps: Vec<StepResult>,
}

impl Report {
    fn from_steps(steps: Vec<StepResult>) -> Self {
        let passed = steps.iter().all(|s| s.status == StepStatus::Pass);
        Self { passed, steps }
    }
}

// ── Observations ──────────────────────────────────────────────────────────────

/// Raw protocol events the embedded host records. Pure data — no validation.
#[derive(Debug, Default)]
pub struct Observations {
    pub hello: Option<HelloParams>,
    pub types_declared: Vec<crate::transport::TypeDeclaration>,
    pub publishes: Vec<(String, TypedValue)>,
    pub tools_declared: Vec<ToolDeclaration>,
    /// Result payload returned by the plugin's `tools/call` handler when
    /// the host invoked [`Scenario::tool_name`]. `None` means the call
    /// never completed (timeout or error).
    pub tool_call_result: Option<serde_json::Value>,
    /// Error from the plugin's `tools/call`, if any. Populated when the
    /// plugin returned a JSON-RPC error instead of a result.
    pub tool_call_error: Option<String>,
}

/// Compare observations against the scenario. Pure function — testable
/// without spinning up the host.
pub fn check(obs: &Observations, scenario: &Scenario) -> Report {
    let mut steps = Vec::new();

    // Step 1: hello arrived with correct id
    match &obs.hello {
        Some(h) if h.plugin_id == scenario.plugin_id => steps.push(StepResult {
            name: "hello",
            status: StepStatus::Pass,
            detail: format!("plugin_id={}", h.plugin_id),
        }),
        Some(h) => steps.push(StepResult {
            name: "hello",
            status: StepStatus::Fail,
            detail: format!(
                "plugin_id was {}; expected {}",
                h.plugin_id, scenario.plugin_id
            ),
        }),
        None => steps.push(StepResult {
            name: "hello",
            status: StepStatus::Fail,
            detail: "plugin never sent orca/hello".into(),
        }),
    }

    // Step 2: exactly one type declared, with the expected name
    if obs.types_declared.len() == 1 && obs.types_declared[0].type_name == scenario.type_name {
        steps.push(StepResult {
            name: "types.declare",
            status: StepStatus::Pass,
            detail: format!("declared {}", obs.types_declared[0].type_name),
        });
    } else {
        steps.push(StepResult {
            name: "types.declare",
            status: StepStatus::Fail,
            detail: format!(
                "expected one type named '{}'; got {} type(s): {:?}",
                scenario.type_name,
                obs.types_declared.len(),
                obs.types_declared
                    .iter()
                    .map(|t| t.type_name.as_str())
                    .collect::<Vec<_>>()
            ),
        });
    }

    // Step 3: published a value to the right context with manifest id echoed
    let expected_type_id = format!("{}.{}", scenario.plugin_id, scenario.type_name);
    let publish_match = obs.publishes.iter().find(|(ctx, val)| {
        ctx == scenario.context_id
            && val.type_id == expected_type_id
            && val
                .payload
                .get(scenario.manifest_id_payload_key)
                .and_then(|v| v.as_str())
                == Some(scenario.plugin_id)
    });
    match publish_match {
        Some((_, val)) => steps.push(StepResult {
            name: "context.publish",
            status: StepStatus::Pass,
            detail: format!("published {} to {}", val.type_id, scenario.context_id),
        }),
        None => steps.push(StepResult {
            name: "context.publish",
            status: StepStatus::Fail,
            detail: format!(
                "no matching publish: expected ({}, type_id={}, payload.{}={}). got: {:?}",
                scenario.context_id,
                expected_type_id,
                scenario.manifest_id_payload_key,
                scenario.plugin_id,
                obs.publishes
            ),
        }),
    }

    // Step 4: tools.declare — exactly one tool with the expected name
    if obs.tools_declared.len() == 1 && obs.tools_declared[0].name == scenario.tool_name {
        steps.push(StepResult {
            name: "tools.declare",
            status: StepStatus::Pass,
            detail: format!("declared {}", obs.tools_declared[0].name),
        });
    } else {
        steps.push(StepResult {
            name: "tools.declare",
            status: StepStatus::Fail,
            detail: format!(
                "expected one tool named '{}'; got {} tool(s): {:?}",
                scenario.tool_name,
                obs.tools_declared.len(),
                obs.tools_declared
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
            ),
        });
    }

    // Step 5: tools.call round-trip — host called the tool, plugin echoed
    // the argument value at the expected key.
    let echoed = obs
        .tool_call_result
        .as_ref()
        .and_then(|v| v.get(scenario.tool_result_echo_key))
        .and_then(|v| v.as_str());
    match echoed {
        Some(s) if s == scenario.tool_call_argument_value => steps.push(StepResult {
            name: "tools.call",
            status: StepStatus::Pass,
            detail: format!("echoed '{s}'"),
        }),
        _ => steps.push(StepResult {
            name: "tools.call",
            status: StepStatus::Fail,
            detail: format!(
                "expected payload.{}={}; got result={:?}, error={:?}",
                scenario.tool_result_echo_key,
                scenario.tool_call_argument_value,
                obs.tool_call_result,
                obs.tool_call_error,
            ),
        }),
    }

    Report::from_steps(steps)
}

// ── Embedded conformance host ─────────────────────────────────────────────────

use rustls::ServerConfig;
use rustls::server::WebPkiClientVerifier;
use tokio_rustls::TlsAcceptor;

/// Boot a minimal in-process host on an ephemeral port that records every
/// hello/types.declare/context.publish it sees. Returns the bound address and
/// a future that resolves with the [`Observations`] once `deadline` elapses
/// (or the connection ends).
///
/// The host is intentionally tiny — it implements only the methods the
/// scenario exercises and stores nothing persistently. For full host
/// behavior, see `orca::plugin_host` in `projects/server`.
pub async fn boot_observation_host(
    pki_dir: &Path,
) -> Result<(SocketAddr, mpsc::UnboundedReceiver<Event>)> {
    let server_bundle = pki::load_server(pki_dir).context("load server TLS bundle")?;
    let acceptor = build_acceptor(&server_bundle)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind conformance host")?;
    let addr = listener.local_addr()?;

    let (event_tx, event_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            let (tcp, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let acceptor = acceptor.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    let _ = handle_observation_conn(tls, event_tx).await;
                }
            });
        }
    });

    Ok((addr, event_rx))
}

/// Single event observed by the conformance host.
#[derive(Debug, Clone)]
pub enum Event {
    Hello(HelloParams),
    TypesDeclared(Vec<crate::transport::TypeDeclaration>),
    Published {
        context_id: String,
        value: TypedValue,
    },
    ToolsDeclared(Vec<ToolDeclaration>),
    ToolCallResult {
        result: Option<serde_json::Value>,
        error: Option<String>,
    },
}

fn build_acceptor(bundle: &pki::NodeBundle) -> Result<TlsAcceptor> {
    let (cert_chain, private_key) = pki::parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem)?;
    let ca_root_store = Arc::new(pki::ca_root_store(&bundle.ca_cert_pem)?);
    let client_cert_verifier = WebPkiClientVerifier::builder(ca_root_store)
        .build()
        .context("build client cert verifier")?;
    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_cert_verifier)
        .with_single_cert(cert_chain, private_key)
        .context("build server TLS config")?;
    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

async fn handle_observation_conn(
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    event_tx: mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(tls);
    let mut hello_seen = false;
    let mut outbound_tool_call_id: Option<serde_json::Value> = None;

    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(f) => f,
            Err(_) => break,
        };
        let msg: Message = match serde_json::from_slice(&frame) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg {
            Message::Request(req) => {
                let id = req.id.clone();
                let method = req.method.clone();
                let response = match req.method.as_str() {
                    "orca/hello" => {
                        let params: HelloParams = serde_json::from_value(
                            req.params.unwrap_or_default(),
                        )
                        .unwrap_or_else(|_| HelloParams {
                            sdk_version: String::new(),
                            plugin_id: String::new(),
                            plugin_version: String::new(),
                            flavor: crate::Flavor::Headless,
                            core_min_required: "0.0.0".into(),
                            methods_required: vec![],
                            methods_optional: vec![],
                            plugins_required: vec![],
                            plugins_optional: vec![],
                        });
                        let _ = event_tx.send(Event::Hello(params.clone()));
                        hello_seen = true;
                        let result = HelloResult {
                            server_version: crate::SDK_VERSION.to_string(),
                            ok: true,
                            status: "full".into(),
                            methods: vec![
                                "orca/hello".into(),
                                "orca/types.declare".into(),
                                "orca/context.publish".into(),
                                TOOLS_DECLARE_METHOD.into(),
                                TOOLS_CALL_METHOD.into(),
                            ],
                            reason: None,
                        };
                        Response::ok(id, serde_json::to_value(&result)?)
                    }
                    "orca/types.declare" => {
                        if !hello_seen {
                            Response::err(
                                id,
                                ErrorObject::invalid_params("orca/hello required first"),
                            )
                        } else {
                            let params: TypesDeclareParams =
                                serde_json::from_value(req.params.unwrap_or_default())?;
                            let plugin_id = SCENARIO.plugin_id;
                            let accepted: Vec<String> = params
                                .types
                                .iter()
                                .map(|t| format!("{plugin_id}.{}", t.type_name))
                                .collect();
                            let _ = event_tx.send(Event::TypesDeclared(params.types));
                            Response::ok(
                                id,
                                serde_json::to_value(&TypesDeclareResult { accepted })?,
                            )
                        }
                    }
                    "orca/context.publish" => {
                        if !hello_seen {
                            Response::err(
                                id,
                                ErrorObject::invalid_params("orca/hello required first"),
                            )
                        } else {
                            let params: ContextPublishParams =
                                serde_json::from_value(req.params.unwrap_or_default())?;
                            let _ = event_tx.send(Event::Published {
                                context_id: params.context_id,
                                value: params.value,
                            });
                            Response::ok(id, serde_json::json!({"ok": true}))
                        }
                    }
                    m if m == TOOLS_DECLARE_METHOD => {
                        if !hello_seen {
                            Response::err(
                                id,
                                ErrorObject::invalid_params("orca/hello required first"),
                            )
                        } else {
                            let params: ToolsDeclareParams =
                                serde_json::from_value(req.params.unwrap_or_default())?;
                            let plugin_id = SCENARIO.plugin_id;
                            let accepted: Vec<String> = params
                                .tools
                                .iter()
                                .map(|t| format!("{plugin_id}.{}", t.name))
                                .collect();
                            let _ = event_tx.send(Event::ToolsDeclared(params.tools));
                            Response::ok(
                                id,
                                serde_json::to_value(&ToolsDeclareResult { accepted })?,
                            )
                        }
                    }
                    other => Response::err(id, ErrorObject::method_not_found(other)),
                };

                let bytes = serde_json::to_vec(&response)?;
                write_frame(&mut writer, &bytes).await?;

                // After accepting the plugin's tools/declare, immediately
                // call back into the plugin to exercise its dispatch path.
                if method == TOOLS_DECLARE_METHOD && outbound_tool_call_id.is_none() {
                    let call_id = serde_json::json!(9001u64);
                    outbound_tool_call_id = Some(call_id.clone());
                    let params = ToolCallParams {
                        name: SCENARIO.tool_name.into(),
                        arguments: serde_json::json!({
                            SCENARIO.tool_call_argument_key: SCENARIO.tool_call_argument_value,
                        }),
                    };
                    let req = Request {
                        jsonrpc: "2.0".into(),
                        id: call_id,
                        method: TOOLS_CALL_METHOD.into(),
                        params: Some(serde_json::to_value(&params)?),
                    };
                    let bytes = serde_json::to_vec(&req)?;
                    write_frame(&mut writer, &bytes).await?;
                }
            }
            Message::Response(resp) => {
                // Only one outbound request from the conformance host today —
                // the tools/call we just sent. Match by id.
                if Some(&resp.id) == outbound_tool_call_id.as_ref() {
                    let (result, error) = match (resp.result, resp.error) {
                        (Some(r), _) => {
                            // ToolCallResult wraps the payload at "result".
                            let payload = r.get("result").cloned();
                            (payload, None)
                        }
                        (_, Some(e)) => (None, Some(e.message)),
                        _ => (None, Some("response missing both result and error".into())),
                    };
                    let _ = event_tx.send(Event::ToolCallResult { result, error });
                }
            }
            Message::Notification(_) => {}
        }
    }
    Ok(())
}

/// Drain the event channel for `timeout`, returning the [`Observations`]
/// collected. Runner-agnostic — works with subprocess or in-process plugins.
pub async fn collect_observations(
    mut rx: mpsc::UnboundedReceiver<Event>,
    timeout: Duration,
) -> Observations {
    let obs = Arc::new(StdMutex::new(Observations::default()));
    let obs_inner = obs.clone();
    let _ = tokio::time::timeout(timeout, async move {
        while let Some(event) = rx.recv().await {
            let mut o = obs_inner.lock().unwrap();
            match event {
                Event::Hello(h) => o.hello = Some(h),
                Event::TypesDeclared(t) => o.types_declared.extend(t),
                Event::Published { context_id, value } => o.publishes.push((context_id, value)),
                Event::ToolsDeclared(t) => o.tools_declared.extend(t),
                Event::ToolCallResult { result, error } => {
                    o.tool_call_result = result;
                    o.tool_call_error = error;
                }
            }
            if o.hello.is_some()
                && !o.types_declared.is_empty()
                && !o.publishes.is_empty()
                && !o.tools_declared.is_empty()
                && (o.tool_call_result.is_some() || o.tool_call_error.is_some())
            {
                break;
            }
        }
    })
    .await;

    Arc::try_unwrap(obs).unwrap().into_inner().unwrap()
}

// ── Subprocess runner ─────────────────────────────────────────────────────────

/// Configuration for a subprocess conformance run.
#[derive(Debug, Clone)]
pub struct SubprocessConfig {
    /// Path to the candidate plugin executable.
    pub plugin_binary: PathBuf,
    /// Working directory the plugin runs in. PKI material and the manifest
    /// fixture are written here; if `None` a tempdir is created.
    pub workdir: Option<PathBuf>,
    /// How long to wait for the plugin to complete the scenario.
    pub timeout: Duration,
}

impl SubprocessConfig {
    pub fn new(plugin_binary: impl Into<PathBuf>) -> Self {
        Self {
            plugin_binary: plugin_binary.into(),
            workdir: None,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Run a candidate plugin binary against the scenario. Returns a [`Report`]
/// describing pass/fail per step. Suitable for CI in any language port —
/// invoke with the path to the language port's hello-world plugin.
pub async fn run_subprocess(cfg: SubprocessConfig) -> Result<Report> {
    let tempdir;
    let workdir: &Path = match &cfg.workdir {
        Some(p) => p.as_path(),
        None => {
            tempdir = tempfile::tempdir().context("create conformance tempdir")?;
            tempdir.path()
        }
    };

    // PKI material: CA + server cert + plugin cert (CN = SCENARIO.plugin_id)
    pki::init(workdir).context("conformance pki init")?;
    let _bundle = pki::issue(workdir, SCENARIO.plugin_id, pki::Capability::General)
        .context("issue conformance plugin cert")?;

    // Write the canonical manifest fixture
    let manifest_path = workdir.join(crate::manifest::Manifest::FILENAME);
    std::fs::write(&manifest_path, SCENARIO_MANIFEST).context("write conformance manifest")?;

    // Boot host
    let (addr, event_rx) = boot_observation_host(workdir).await?;
    let observe = collect_observations(event_rx, cfg.timeout);

    // Spawn plugin
    let mut cmd = tokio::process::Command::new(&cfg.plugin_binary);
    cmd.env("ORCA_PLUGIN_ADDR", addr.to_string())
        .env("ORCA_PKI_DIR", workdir)
        .env("ORCA_PLUGIN_ID", SCENARIO.plugin_id)
        .env("ORCA_MANIFEST_PATH", &manifest_path)
        .current_dir(workdir);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn plugin binary {}", cfg.plugin_binary.display()))?;

    // Plugin runs concurrently with observation collection. We don't block
    // the report on plugin exit — the scenario is "complete" when the host
    // has seen hello + types.declare + publish, even if the plugin is still
    // alive (e.g. holding the connection open).
    let plugin_handle = tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let observations = observe.await;
    plugin_handle.abort();

    Ok(check(&observations, &SCENARIO))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolHandler, ToolHandlerError};
    use crate::transport::{Sensitivity, TcpTransport, TypeDeclaration};
    use std::sync::Arc;

    fn install_ring() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    // ── Pure checker tests ───────────────────────────────────────────────────

    fn good_observations() -> Observations {
        Observations {
            hello: Some(HelloParams {
                sdk_version: "0.1.0".into(),
                plugin_id: SCENARIO.plugin_id.into(),
                plugin_version: String::new(),
                flavor: crate::Flavor::Headless,
                core_min_required: "0.1.0".into(),
                methods_required: vec![],
                methods_optional: vec![],
                plugins_required: vec![],
                plugins_optional: vec![],
            }),
            types_declared: vec![TypeDeclaration {
                type_name: SCENARIO.type_name.into(),
                schema_version: SCENARIO.type_schema_version.into(),
                schema: serde_json::from_str(SCENARIO.type_schema_json).unwrap(),
                sensitivity: Sensitivity::General,
            }],
            publishes: vec![(
                SCENARIO.context_id.into(),
                TypedValue {
                    type_id: format!("{}.{}", SCENARIO.plugin_id, SCENARIO.type_name),
                    schema_version: SCENARIO.type_schema_version.into(),
                    sensitivity: Sensitivity::General,
                    payload: serde_json::json!({
                        "text": "hi",
                        "manifest_id": SCENARIO.plugin_id,
                    }),
                },
            )],
            tools_declared: vec![ToolDeclaration {
                name: SCENARIO.tool_name.into(),
                description: "echo".into(),
                input_schema: serde_json::from_str(SCENARIO.tool_input_schema_json).unwrap(),
                sensitivity: Sensitivity::General,
            }],
            tool_call_result: Some(serde_json::json!({
                SCENARIO.tool_result_echo_key: SCENARIO.tool_call_argument_value,
            })),
            tool_call_error: None,
        }
    }

    #[test]
    fn check_passes_for_good_observations() {
        let report = check(&good_observations(), &SCENARIO);
        assert!(
            report.passed,
            "expected pass, got steps: {:?}",
            report.steps
        );
        assert_eq!(report.steps.len(), 5);
    }

    #[test]
    fn check_fails_when_tool_not_declared() {
        let mut o = good_observations();
        o.tools_declared.clear();
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
        let step = report
            .steps
            .iter()
            .find(|s| s.name == "tools.declare")
            .unwrap();
        assert_eq!(step.status, StepStatus::Fail);
    }

    #[test]
    fn check_fails_when_tool_call_echo_wrong() {
        let mut o = good_observations();
        o.tool_call_result = Some(serde_json::json!({SCENARIO.tool_result_echo_key: "wrong"}));
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
        let step = report
            .steps
            .iter()
            .find(|s| s.name == "tools.call")
            .unwrap();
        assert_eq!(step.status, StepStatus::Fail);
    }

    #[test]
    fn check_fails_when_tool_call_errored() {
        let mut o = good_observations();
        o.tool_call_result = None;
        o.tool_call_error = Some("plugin returned an error".into());
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
    }

    #[test]
    fn check_fails_when_hello_missing() {
        let mut o = good_observations();
        o.hello = None;
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
        let hello = report.steps.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello.status, StepStatus::Fail);
    }

    #[test]
    fn check_fails_when_plugin_id_wrong() {
        let mut o = good_observations();
        o.hello.as_mut().unwrap().plugin_id = "wrong".into();
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
    }

    #[test]
    fn check_fails_when_no_types_declared() {
        let mut o = good_observations();
        o.types_declared.clear();
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
    }

    #[test]
    fn check_fails_when_publish_payload_missing_manifest_id() {
        let mut o = good_observations();
        o.publishes[0].1.payload = serde_json::json!({"text": "hi"});
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
    }

    #[test]
    fn check_fails_when_publish_to_wrong_context() {
        let mut o = good_observations();
        o.publishes[0].0 = "wrong:context".into();
        let report = check(&o, &SCENARIO);
        assert!(!report.passed);
    }

    // ── Live host + in-process plugin (no subprocess) ────────────────────────

    /// Drive the embedded host with an in-process plugin written using the
    /// SDK's own TcpTransport. Validates host wiring without needing a
    /// language-port binary.
    #[tokio::test(flavor = "current_thread")]
    async fn embedded_host_observes_inprocess_plugin() {
        install_ring();

        let dir = tempfile::tempdir().unwrap();
        let pki_dir = dir.path();
        pki::init(pki_dir).unwrap();
        let bundle = pki::issue(pki_dir, SCENARIO.plugin_id, pki::Capability::General).unwrap();

        let (addr, event_rx) = boot_observation_host(pki_dir).await.unwrap();

        // In-process "plugin": connect, hello, declare types, publish, register
        // the echo tool, declare it, then idle so its reader task can serve
        // the host's incoming tools/call.
        let plugin_task = tokio::spawn(async move {
            let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
            transport
                .hello(SCENARIO.plugin_id, crate::Flavor::Headless, vec![], vec![])
                .await
                .unwrap();
            transport
                .declare_types(vec![TypeDeclaration {
                    type_name: SCENARIO.type_name.into(),
                    schema_version: SCENARIO.type_schema_version.into(),
                    schema: serde_json::from_str(SCENARIO.type_schema_json).unwrap(),
                    sensitivity: Sensitivity::General,
                }])
                .await
                .unwrap();
            transport
                .publish_context(
                    SCENARIO.context_id,
                    TypedValue {
                        type_id: format!("{}.{}", SCENARIO.plugin_id, SCENARIO.type_name),
                        schema_version: SCENARIO.type_schema_version.into(),
                        sensitivity: Sensitivity::General,
                        payload: serde_json::json!({
                            "text": "hello",
                            "manifest_id": SCENARIO.plugin_id,
                        }),
                    },
                )
                .await
                .unwrap();

            let echo: Arc<dyn ToolHandler> = Arc::new(|args: serde_json::Value| async move {
                let v = args
                    .get(SCENARIO.tool_call_argument_key)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolHandlerError::new("missing 'value'"))?
                    .to_string();
                Ok(serde_json::json!({SCENARIO.tool_result_echo_key: v}))
            });
            transport.register_tool(
                SCENARIO.tool_name,
                "echo back the value argument",
                serde_json::from_str(SCENARIO.tool_input_schema_json).unwrap(),
                Sensitivity::General,
                echo,
            );
            transport.declare_tools().await.unwrap();

            // Hold the connection open so the reader task can serve the
            // host's incoming tools/call. Drop happens when test ends.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let observations = collect_observations(event_rx, Duration::from_secs(5)).await;
        plugin_task.await.unwrap();

        let report = check(&observations, &SCENARIO);
        assert!(
            report.passed,
            "in-process plugin should be conformant, got: {:?}",
            report.steps
        );
    }

    /// Negative case: a plugin that does hello + types.declare but never
    /// publishes is correctly reported as non-conformant.
    #[tokio::test(flavor = "current_thread")]
    async fn embedded_host_reports_nonconformant_plugin() {
        install_ring();

        let dir = tempfile::tempdir().unwrap();
        let pki_dir = dir.path();
        pki::init(pki_dir).unwrap();
        let bundle = pki::issue(pki_dir, SCENARIO.plugin_id, pki::Capability::General).unwrap();

        let (addr, event_rx) = boot_observation_host(pki_dir).await.unwrap();

        let plugin_task = tokio::spawn(async move {
            let transport = TcpTransport::connect(addr, &bundle).await.unwrap();
            transport
                .hello(SCENARIO.plugin_id, crate::Flavor::Headless, vec![], vec![])
                .await
                .unwrap();
            transport
                .declare_types(vec![TypeDeclaration {
                    type_name: SCENARIO.type_name.into(),
                    schema_version: SCENARIO.type_schema_version.into(),
                    schema: serde_json::from_str(SCENARIO.type_schema_json).unwrap(),
                    sensitivity: Sensitivity::General,
                }])
                .await
                .unwrap();
            // ...but never publishes.
        });

        let observations = collect_observations(event_rx, Duration::from_millis(500)).await;
        plugin_task.await.unwrap();

        let report = check(&observations, &SCENARIO);
        assert!(!report.passed);
        let pub_step = report
            .steps
            .iter()
            .find(|s| s.name == "context.publish")
            .unwrap();
        assert_eq!(pub_step.status, StepStatus::Fail);
    }
}
