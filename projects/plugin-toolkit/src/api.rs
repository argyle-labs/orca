#![allow(clippy::disallowed_types)]

use anyhow::{Result, anyhow};
use plugin_proto::{ApiCall, ApiCallPlan};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::capsink::cap_route;

pub(crate) const PLANNED_MARKER: &str = "\u{1e}api_call_planned\u{1e}";

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub op: String,
    pub plan: ApiCallPlan,
}

impl std::fmt::Display for Planned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: planned, not executed: {} {}",
            self.op, self.plan.method, self.plan.url
        )
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
        Err(e) => match planned(&e.to_string()) {
            Some(plan) => Err(Planned {
                op: op.to_string(),
                plan,
            }
            .into()),
            None => Err(e),
        },
    }
}

fn planned(message: &str) -> Option<ApiCallPlan> {
    let body = message
        .strip_prefix("capability api.call: ")?
        .strip_prefix(PLANNED_MARKER)?;
    serde_json::from_str(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsink::with_cap_sink;
    use serde_json::json;

    fn plan_json() -> Value {
        json!({
            "method": "POST",
            "url": "https://h/x",
            "query": [],
            "body": null,
            "endpoint": "main",
            "route": "https://h"
        })
    }

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
            Box::new(|_, _| Err(format!("{PLANNED_MARKER}{}", plan_json()))),
            || call::<Value>("p.make", "main", json!({})),
        )
        .unwrap_err();
        let planned = err.downcast_ref::<Planned>().expect("typed Planned error");
        assert_eq!(planned.plan.method, "POST");
        assert_eq!(planned.op, "p.make");
    }

    #[test]
    fn lookalike_errors_are_not_plans() {
        for msg in [
            format!("{PLANNED_MARKER}{{\"method\":\"POST\"}}"),
            format!("upstream said {PLANNED_MARKER}{}", plan_json()),
            format!("{PLANNED_MARKER}{}", json!({"extra": 1, "method": "GET"})),
            "forbidden".to_string(),
        ] {
            let err = with_cap_sink(Box::new(move |_, _| Err(msg.clone())), || {
                call::<Value>("p.make", "main", json!({}))
            })
            .unwrap_err();
            assert!(err.downcast_ref::<Planned>().is_none(), "{err}");
        }
        assert!(call::<Value>("p.get", "main", json!({})).is_err());
    }
}
