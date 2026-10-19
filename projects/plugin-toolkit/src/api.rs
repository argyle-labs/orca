#![allow(clippy::disallowed_types)]

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::capsink::cap_route;
use plugin_proto::ApiCall;

pub fn call<T: DeserializeOwned>(op: &str, endpoint: &str, args: Value) -> Result<T> {
    let req = ApiCall {
        surface: None,
        op: op.to_string(),
        endpoint: endpoint.to_string(),
        args,
    };
    let reply = cap_route("api.call", &serde_json::to_string(&req)?).ok_or_else(|| {
        anyhow!("api.call capability unavailable: this plugin is not running as an orca subprocess")
    })??;
    let reply: Value = serde_json::from_str(&reply)?;
    match reply.get("mode").and_then(Value::as_str) {
        Some("response") => Ok(serde_json::from_value(
            reply.get("result").cloned().unwrap_or(Value::Null),
        )?),
        Some("plan") => bail!("{op}: dry run planned {}", reply["result"]),
        _ => bail!("{op}: malformed api.call reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsink::with_cap_sink;
    use serde_json::json;

    #[test]
    fn call_routes_through_the_sink_and_decodes_the_response() {
        let out: Value = with_cap_sink(
            Box::new(|cap, op| {
                assert_eq!(cap, "api.call");
                let req: ApiCall = serde_json::from_str(op).unwrap();
                assert_eq!(
                    req,
                    ApiCall {
                        surface: None,
                        op: "p.get".into(),
                        endpoint: "main".into(),
                        args: json!({"id": 1}),
                    }
                );
                Ok(json!({"mode": "response", "result": {"ok": true}}).to_string())
            }),
            || call("p.get", "main", json!({"id": 1})),
        )
        .unwrap();
        assert_eq!(out, json!({"ok": true}));
    }

    #[test]
    fn call_surfaces_plans_and_errors() {
        let plan = with_cap_sink(
            Box::new(|_, _| Ok(json!({"mode": "plan", "result": {"method": "POST"}}).to_string())),
            || call::<Value>("p.make", "main", json!({})),
        );
        assert!(plan.unwrap_err().to_string().contains("dry run"));
        let denied = with_cap_sink(Box::new(|_, _| Err("forbidden".into())), || {
            call::<Value>("p.make", "main", json!({}))
        });
        assert!(denied.unwrap_err().to_string().contains("forbidden"));
        assert!(call::<Value>("p.get", "main", json!({})).is_err());
    }
}
