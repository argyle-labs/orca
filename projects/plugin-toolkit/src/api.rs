#![allow(clippy::disallowed_types)]

use anyhow::{Result, anyhow};
use plugin_proto::{API_CALL_PLANNED, ApiCall};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::capsink::cap_route;

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub op: String,
    pub plan: Value,
}

impl std::fmt::Display for Planned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: planned, not executed: {}", self.op, self.plan)
    }
}

impl std::error::Error for Planned {}

pub fn call<T: DeserializeOwned>(op: &str, endpoint: &str, args: Value) -> Result<T> {
    let req = ApiCall {
        surface: None,
        op: op.to_string(),
        endpoint: endpoint.to_string(),
        args,
    };
    let reply = cap_route("api.call", &serde_json::to_string(&req)?).ok_or_else(|| {
        anyhow!("api.call capability unavailable: this plugin is not running as an orca subprocess")
    })?;
    match reply {
        Ok(body) => Ok(serde_json::from_str(&body)?),
        Err(e) => {
            let msg = e.to_string();
            match msg.split_once(API_CALL_PLANNED) {
                Some((_, plan)) => Err(Planned {
                    op: op.to_string(),
                    plan: serde_json::from_str(plan).unwrap_or(Value::String(plan.to_string())),
                }
                .into()),
                None => Err(e),
            }
        }
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
                Ok(json!({"ok": true}).to_string())
            }),
            || call("p.get", "main", json!({"id": 1})),
        )
        .unwrap();
        assert_eq!(out, json!({"ok": true}));
    }

    #[test]
    fn a_planned_call_is_a_typed_error_never_a_value() {
        let err = with_cap_sink(
            Box::new(|_, _| Err(format!("{API_CALL_PLANNED}{}", json!({"method": "POST"})))),
            || call::<Value>("p.make", "main", json!({})),
        )
        .unwrap_err();
        let planned = err.downcast_ref::<Planned>().expect("typed Planned error");
        assert_eq!(planned.plan, json!({"method": "POST"}));
        assert_eq!(planned.op, "p.make");
    }

    #[test]
    fn denials_and_missing_sink_are_errors() {
        let denied = with_cap_sink(Box::new(|_, _| Err("forbidden".into())), || {
            call::<Value>("p.make", "main", json!({}))
        });
        let err = denied.unwrap_err();
        assert!(err.downcast_ref::<Planned>().is_none());
        assert!(err.to_string().contains("forbidden"));
        assert!(call::<Value>("p.get", "main", json!({})).is_err());
    }
}
