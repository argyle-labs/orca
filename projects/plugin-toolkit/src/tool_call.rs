#![allow(clippy::disallowed_types)]

use anyhow::{Result, anyhow};
use plugin_proto::ToolCall;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::capsink::{cap_route, core_advertises};

pub fn available() -> bool {
    core_advertises(crate::abi::TOOL_CALL)
}

pub fn call<T: DeserializeOwned>(tool: &str, args: Value) -> Result<T> {
    Ok(serde_json::from_value(call_json(tool, args)?)?)
}

pub fn call_json(tool: &str, args: Value) -> Result<Value> {
    if !available() {
        return Err(anyhow!("tool.call unavailable: core does not advertise it"));
    }
    let req = ToolCall {
        tool: tool.to_string(),
        args,
    };
    let reply =
        cap_route(crate::abi::TOOL_CALL, &serde_json::to_string(&req)?).ok_or_else(|| {
            anyhow!(
                "tool.call capability unavailable: this plugin is not running as an orca subprocess"
            )
        })??;
    Ok(serde_json::from_str(&reply)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsink::{set_core_privileged, with_cap_sink};
    use serde_json::json;

    #[test]
    fn call_routes_the_tool_and_args_through_the_sink() {
        set_core_privileged(&[crate::abi::TOOL_CALL.to_string()]);
        let out: Value = with_cap_sink(
            Box::new(|cap, op| {
                assert_eq!(cap, crate::abi::TOOL_CALL);
                let req: ToolCall = serde_json::from_str(op).unwrap();
                assert_eq!(req.tool, "service.create");
                assert_eq!(req.args, json!({ "action": "deploy" }));
                Ok(json!({ "ok": true }).to_string())
            }),
            || call("service.create", json!({ "action": "deploy" })),
        )
        .unwrap();
        assert_eq!(out, json!({ "ok": true }));
    }

    #[test]
    fn a_refusal_is_an_error() {
        set_core_privileged(&[crate::abi::TOOL_CALL.to_string()]);
        let err = with_cap_sink(Box::new(|_, _| Err("requires role 'admin'".into())), || {
            call_json("service.create", json!({}))
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("requires role 'admin'"), "{err}");
    }

    #[test]
    fn unadvertised_core_is_refused_before_sending() {
        set_core_privileged(&[]);
        let err = with_cap_sink(Box::new(|_, _| panic!("must not send")), || {
            call_json("service.create", json!({}))
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not advertise"), "{err}");
    }
}
