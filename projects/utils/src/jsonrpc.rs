//! JSON-RPC 2.0 wire types shared between the SDK (plugin side) and the server
//! plugin host.
// JSON-RPC 2.0 protocol envelopes (id/params/result/error.data) are inherently
// opaque — the spec mandates Value at the boundary.
#![allow(clippy::disallowed_types)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    pub fn invalid_params(detail: &str) -> Self {
        Self {
            code: -32602,
            message: format!("invalid params: {detail}"),
            data: None,
        }
    }

    pub fn internal(detail: &str) -> Self {
        Self {
            code: -32603,
            message: format!("internal error: {detail}"),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// Any message received from the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Response(Response),
    Notification(Notification),
    // A request from the server back to the plugin is possible in future; ignore for now.
    Request(Request),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrips() {
        let req = Request::new(1u64, "orca/hello", Some(json!({"sdk_version": "0.1.0"})));
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, "orca/hello");
        assert_eq!(back.id, json!(1));
    }

    #[test]
    fn response_ok_roundtrips() {
        let r = Response::ok(json!(1), json!({"ok": true}));
        let json = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(!back.is_error());
        assert_eq!(back.result.unwrap()["ok"], json!(true));
    }

    #[test]
    fn response_err_roundtrips() {
        let r = Response::err(json!(1), ErrorObject::method_not_found("foo/bar"));
        let json = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(back.is_error());
        assert_eq!(back.error.unwrap().code, -32601);
    }

    #[test]
    fn message_deserialized_as_response() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert!(matches!(msg, Message::Response(_)));
    }

    #[test]
    fn message_deserialized_as_notification() {
        let raw = r#"{"jsonrpc":"2.0","method":"orca/ping"}"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert!(matches!(msg, Message::Notification(_)));
    }
}
