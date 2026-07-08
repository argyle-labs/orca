//! Daemon-side capability host — the counterpart to a subprocess plugin's
//! capability sink (`plugin_toolkit::serve`).
//!
//! When the supervisor reads a [`Cap`](plugin_proto::Frame::Cap) frame from a
//! plugin, it calls [`handle_cap`] to execute the request against orca's own
//! services and returns the result as a
//! [`CapResult`](plugin_proto::Frame::CapResult). A plugin thus delegates DB /
//! secret access (and, later, HTTP / transport) instead of linking its own —
//! the whole point of the thin-plugin model.
//!
//! These route to the SAME pooled executors the in-process toolkit falls back
//! to (`db::plugin_tables::exec_db_op_pooled` / `db::secrets::exec_secret_op_pooled`),
//! so a tool behaves identically whether its plugin is loaded in-process or run
//! as a subprocess.

use anyhow::{Result, anyhow};
use plugin_toolkit::abi::{DbOp, SecretOp};
use plugin_toolkit::serde_json::{self, Value};

/// Capability names the daemon serves. Advertised in the handshake `Welcome`.
pub const CAPABILITIES: &[&str] = &["db.op", "secret.op"];

/// Execute one capability request. `args` is the op payload the plugin sent
/// (a serialized [`DbOp`] / [`SecretOp`]); the returned `Value` is the reply the
/// supervisor wraps into a `CapResult`.
///
/// HTTP stays in-process in the plugin for now (progenitor clients link
/// `reqwest`); `http.request` joins this match when that's shed.
pub fn handle_cap(cap: &str, args: Value) -> Result<Value> {
    match cap {
        "db.op" => {
            let op: DbOp =
                serde_json::from_value(args).map_err(|e| anyhow!("db.op: bad op payload: {e}"))?;
            let reply = db::plugin_tables::exec_db_op_pooled(&op)?;
            Ok(serde_json::to_value(reply)?)
        }
        "secret.op" => {
            let op: SecretOp = serde_json::from_value(args)
                .map_err(|e| anyhow!("secret.op: bad op payload: {e}"))?;
            let reply = db::secrets::exec_secret_op_pooled(&op)?;
            Ok(serde_json::to_value(reply)?)
        }
        other => Err(anyhow!("unknown capability '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json::json;

    #[test]
    fn unknown_capability_errors() {
        let err = handle_cap("http.request", json!({}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown capability"), "got: {err}");
    }

    #[test]
    fn malformed_op_payload_errors_before_execution() {
        // A db.op whose payload isn't a valid DbOp fails at deserialization —
        // no db needed, so this is a pure routing/validation check.
        let err = handle_cap("db.op", json!({"not": "a valid op"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("db.op: bad op payload"), "got: {err}");
    }

    #[test]
    fn capabilities_list_is_advertised() {
        assert!(CAPABILITIES.contains(&"db.op"));
        assert!(CAPABILITIES.contains(&"secret.op"));
    }
}
