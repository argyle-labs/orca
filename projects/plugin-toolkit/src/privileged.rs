//! Root ops for a plugin on its own host, through orca's privileged seam.
//!
//! Plugins run as the unprivileged orca service user. When one needs root
//! (write a `/boot` template, run a vendor CLI), it does not get a sudo grant
//! of its own. Instead:
//!
//! * [`apply`] sends `{plugin, op, payload}` to `sudo -n $ORCA_BIN admin
//!   plugin-apply`. As root, orca checks that `plugin` is installed and its
//!   binary matches the hash recorded at install, then runs that binary as
//!   `<plugin> --privileged-op` with `{op, payload}` on stdin.
//! * In that mode the plugin's `main` hands stdin to a [`PrivilegedOps`]: a
//!   closed set of named handlers, each validating its own payload. Any other
//!   op is refused.
//!
//! The reply is one `{ok, detail | error}` JSON line ([`PrivilegedReply`]) on
//! stdout, and success needs both exit 0 and `ok: true`.
//!
//! ```ignore
//! fn main() -> anyhow::Result<()> {
//!     if plugin_toolkit::privileged::requested() {
//!         plugin_toolkit::privileged::PrivilegedOps::new()
//!             .op("set_autostart", |p: SetAutostart| async move { set_autostart(p).await })
//!             .serve_stdin();
//!     }
//!     // ... normal plugin serve
//! }
//! ```

// The payload is each op's own schema: it crosses the seam as JSON and is
// deserialized into the handler's typed argument here, at the boundary.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

pub use crate::lxc_exec::ORCA_BIN_ENV;

/// Argument that puts a plugin binary into one-shot privileged mode.
pub const PRIVILEGED_FLAG: &str = "--privileged-op";

/// The one reply line of a privileged op, from the plugin or from the seam.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivilegedReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
struct Request<'a> {
    plugin: &'a str,
    op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a Value>,
}

/// Run `op` with `payload` as root through `sudo -n $ORCA_BIN admin
/// plugin-apply`. Returns the op's `detail` on success.
pub async fn apply<P: Serialize>(plugin: &str, op: &str, payload: Option<&P>) -> Result<String> {
    let orca_bin = std::env::var(ORCA_BIN_ENV).with_context(|| {
        format!("{ORCA_BIN_ENV} is not set, so the orca privileged seam is unavailable")
    })?;
    anyhow::ensure!(
        std::path::Path::new(&orca_bin).is_absolute(),
        "{ORCA_BIN_ENV} must be an absolute path, got {orca_bin:?}"
    );
    let input = request_json(plugin, op, payload)?;
    let mut child = tokio::process::Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg(&orca_bin)
        .args(["admin", "plugin-apply"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn `sudo -n {orca_bin} admin plugin-apply`"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&input)
            .await
            .context("send privileged op")?;
        stdin
            .shutdown()
            .await
            .context("close privileged op stdin")?;
    }
    let out = child
        .wait_with_output()
        .await
        .context("wait for plugin-apply")?;
    parse_reply(
        out.status.success(),
        out.status.code(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )
}

fn request_json<P: Serialize>(plugin: &str, op: &str, payload: Option<&P>) -> Result<Vec<u8>> {
    let payload = payload
        .map(serde_json::to_value)
        .transpose()
        .context("serialize privileged payload")?;
    Ok(serde_json::to_vec(&Request {
        plugin,
        op,
        payload: payload.as_ref(),
    })?)
}

/// Read the seam's outcome: the LAST JSON line of stdout is the reply, and
/// success needs both a zero exit and `ok: true`.
pub fn parse_reply(success: bool, code: Option<i32>, stdout: &str, stderr: &str) -> Result<String> {
    let reply: Option<PrivilegedReply> = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok());
    match reply {
        Some(PrivilegedReply {
            ok: true, detail, ..
        }) if success => Ok(detail.unwrap_or_default()),
        Some(PrivilegedReply { ok: true, .. }) => bail!(
            "privileged seam exited {code:?} despite an ok reply: {}",
            stderr.trim()
        ),
        Some(PrivilegedReply { error, .. }) => {
            bail!("{}", error.unwrap_or_else(|| "privileged op failed".into()))
        }
        None => bail!(
            "privileged seam gave no reply (exit {code:?}): {}",
            stderr.trim()
        ),
    }
}

/// Whether this process was started as `<plugin> --privileged-op`.
pub fn requested() -> bool {
    std::env::args().nth(1).as_deref() == Some(PRIVILEGED_FLAG)
}

type Handler = Box<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<String>>>>>;

/// The closed set of ops a plugin accepts in `--privileged-op` mode.
#[derive(Default)]
pub struct PrivilegedOps {
    ops: BTreeMap<String, Handler>,
}

/// What arrives on stdin. `plugin` is the seam's routing key, ignored here.
#[derive(Deserialize)]
struct Incoming {
    op: String,
    #[serde(default)]
    payload: Value,
}

impl PrivilegedOps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `name`. The payload is deserialized into `T`; an absent payload
    /// is `null`, so a no-payload op takes `()`.
    pub fn op<T, F, Fut>(mut self, name: &str, handler: F) -> Self
    where
        T: DeserializeOwned + 'static,
        F: Fn(T) -> Fut + 'static,
        Fut: Future<Output = Result<String>> + 'static,
    {
        let name_owned = name.to_string();
        let handler = std::rc::Rc::new(handler);
        self.ops.insert(
            name.to_string(),
            Box::new(move |payload: Value| {
                let handler = std::rc::Rc::clone(&handler);
                let name = name_owned.clone();
                Box::pin(async move {
                    let args: T = serde_json::from_value(payload)
                        .with_context(|| format!("invalid payload for op '{name}'"))?;
                    handler(args).await
                })
            }),
        );
        self
    }

    /// Run one request and produce its reply. Unknown ops are refused.
    pub async fn dispatch(&self, input: &str) -> PrivilegedReply {
        let result = async {
            let req: Incoming = serde_json::from_str(input).context("decode privileged request")?;
            let Some(handler) = self.ops.get(&req.op) else {
                bail!("unknown privileged op '{}'", req.op);
            };
            handler(req.payload).await
        }
        .await;
        match result {
            Ok(detail) => PrivilegedReply {
                ok: true,
                detail: Some(detail),
                error: None,
            },
            Err(e) => PrivilegedReply {
                ok: false,
                detail: None,
                error: Some(format!("{e:#}")),
            },
        }
    }

    /// The `--privileged-op` entry: read one request from stdin, print the
    /// reply line, exit 0 on success and 1 otherwise.
    pub fn serve_stdin(&self) -> ! {
        let mut input = String::new();
        let reply = match std::io::Read::read_to_string(&mut std::io::stdin(), &mut input) {
            Ok(_) => crate::reactor::block_on(self.dispatch(&input)),
            Err(e) => PrivilegedReply {
                ok: false,
                detail: None,
                error: Some(format!("read privileged request: {e}")),
            },
        };
        let line = serde_json::to_string(&reply)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"could not encode the reply"}"#.to_string());
        println!("{line}");
        std::process::exit(if reply.ok { 0 } else { 1 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Autostart {
        name: String,
        on: bool,
    }

    fn ops() -> PrivilegedOps {
        PrivilegedOps::new()
            .op("ping", |(): ()| async { Ok("pong".to_string()) })
            .op("set_autostart", |p: Autostart| async move {
                anyhow::ensure!(!p.name.contains('/'), "bad name");
                Ok(format!("{}={}", p.name, p.on))
            })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn known_ops_run_with_typed_payloads() {
        let ops = ops();
        let r = ops.dispatch(r#"{"plugin":"unraid","op":"ping"}"#).await;
        assert_eq!(r.detail.as_deref(), Some("pong"));
        let r = ops
            .dispatch(r#"{"op":"set_autostart","payload":{"name":"pbs","on":true}}"#)
            .await;
        assert!(r.ok);
        assert_eq!(r.detail.as_deref(), Some("pbs=true"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_ops_and_bad_payloads_are_refused() {
        let ops = ops();
        let r = ops.dispatch(r#"{"op":"format_disk"}"#).await;
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("unknown privileged op"));
        let r = ops
            .dispatch(r#"{"op":"set_autostart","payload":{"name":"pbs"}}"#)
            .await;
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("invalid payload"));
        let r = ops
            .dispatch(r#"{"op":"set_autostart","payload":{"name":"../x","on":true}}"#)
            .await;
        assert_eq!(r.error.as_deref(), Some("bad name"));
        assert!(!ops.dispatch("not json").await.ok);
    }

    #[test]
    fn the_request_matches_the_seam_shape() {
        #[derive(Serialize)]
        struct P {
            name: &'static str,
        }
        let body = request_json("unraid", "rebuild", Some(&P { name: "pbs" })).unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"plugin":"unraid","op":"rebuild","payload":{"name":"pbs"}}"#
        );
        let body = request_json::<()>("unraid", "ping", None).unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"plugin":"unraid","op":"ping"}"#
        );
    }

    #[test]
    fn the_last_json_line_decides_and_success_needs_exit_zero_and_ok() {
        let ok = "noise\n{\"ok\":true,\"detail\":\"done\"}\n";
        assert_eq!(parse_reply(true, Some(0), ok, "").unwrap(), "done");
        assert!(parse_reply(false, Some(1), ok, "boom").is_err());
        let refused = "{\"ok\":false,\"error\":\"plugin-apply refused: tampered\"}\n";
        let err = parse_reply(false, Some(1), refused, "").unwrap_err();
        assert!(err.to_string().contains("tampered"));
        let err = parse_reply(false, Some(1), "", "sudo: a password is required").unwrap_err();
        assert!(err.to_string().contains("password is required"));
    }
}
