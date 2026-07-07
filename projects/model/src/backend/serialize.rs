// LLM wire-format serialization helpers; HashMap/Value are protocol-level passthrough.
#![allow(clippy::disallowed_types)]
use crate::types::Message;
use contract::ToolDef;
use serde_json::{Value, json};

// ── Anthropic wire format ─────────────────────────────────────────────────────

pub fn anthropic_messages(messages: &[Message]) -> Value {
    let mut out = vec![];

    for msg in messages {
        match msg {
            Message::User { content } => {
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant { text, tool_calls } => {
                let mut content: Vec<Value> = vec![];
                if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                    content.push(json!({ "type": "text", "text": t }));
                }
                for tc in tool_calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.input,
                    }));
                }
                if !content.is_empty() {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
            }
            Message::ToolResults(results) => {
                let content: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": r.tool_use_id,
                            "content": r.content,
                            "is_error": r.is_error,
                        })
                    })
                    .collect();
                out.push(json!({ "role": "user", "content": content }));
            }
        }
    }

    Value::Array(out)
}

pub fn anthropic_tools(tools: &[ToolDef]) -> Value {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

// ── OpenAI-compatible wire format ─────────────────────────────────────────────

pub fn openai_messages(messages: &[Message], system: &str) -> Value {
    let mut out = vec![];

    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }

    for msg in messages {
        match msg {
            Message::User { content } => {
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant { text, tool_calls } => {
                if tool_calls.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": text.as_deref().unwrap_or(""),
                    }));
                } else {
                    let tc_list: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": serde_json::to_string(&tc.input)
                                        .unwrap_or_default(),
                                },
                            })
                        })
                        .collect();
                    // OpenAI spec: content must be null (not "") when tool_calls is present.
                    // Sending "" breaks some backends (Ollama, several llama.cpp derivatives).
                    let content: Option<&str> = text.as_deref().filter(|t| !t.is_empty());
                    out.push(json!({
                        "role": "assistant",
                        "content": content,
                        "tool_calls": tc_list,
                    }));
                }
            }
            Message::ToolResults(results) => {
                for r in results {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": r.tool_use_id,
                        "content": r.content,
                    }));
                }
            }
        }
    }

    Value::Array(out)
}

pub fn openai_tools(tools: &[ToolDef]) -> Value {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use contract::{ToolCall, ToolResult};

    fn user(s: &str) -> Message {
        Message::User {
            content: s.to_string(),
        }
    }
    fn assistant(text: &str) -> Message {
        Message::Assistant {
            text: Some(text.to_string()),
            tool_calls: vec![],
        }
    }
    fn assistant_with_tool(text: Option<&str>, id: &str, name: &str) -> Message {
        Message::Assistant {
            text: text.map(str::to_string),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input: json!({ "arg": "val" }),
            }],
        }
    }
    fn tool_result(id: &str, content: &str, is_error: bool) -> Message {
        Message::ToolResults(vec![ToolResult {
            tool_use_id: id.to_string(),
            content: content.to_string(),
            is_error,
        }])
    }

    // ── anthropic_messages ────────────────────────────────────────────────────

    #[test]
    fn anthropic_empty_messages_returns_empty_array() {
        let v = anthropic_messages(&[]);
        assert_eq!(v, json!([]));
    }

    #[test]
    fn anthropic_single_user_message() {
        let v = anthropic_messages(&[user("hello")]);
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"], "hello");
    }

    #[test]
    fn anthropic_assistant_text_only() {
        let v = anthropic_messages(&[assistant("hi there")]);
        assert_eq!(v[0]["role"], "assistant");
        assert_eq!(v[0]["content"][0]["type"], "text");
        assert_eq!(v[0]["content"][0]["text"], "hi there");
    }

    #[test]
    fn anthropic_assistant_with_tool_call() {
        let v = anthropic_messages(&[assistant_with_tool(None, "tool-1", "bash")]);
        let content = &v[0]["content"];
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "tool-1");
        assert_eq!(content[0]["name"], "bash");
    }

    #[test]
    fn anthropic_assistant_empty_text_omitted() {
        let msg = Message::Assistant {
            text: Some(String::new()),
            tool_calls: vec![],
        };
        let v = anthropic_messages(&[msg]);
        // Empty text should not appear as a content block — no assistant entry at all.
        assert_eq!(v, json!([]));
    }

    #[test]
    fn anthropic_tool_results_become_user_role() {
        let v = anthropic_messages(&[tool_result("t1", "output", false)]);
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"][0]["type"], "tool_result");
        assert_eq!(v[0]["content"][0]["tool_use_id"], "t1");
        assert_eq!(v[0]["content"][0]["content"], "output");
        assert_eq!(v[0]["content"][0]["is_error"], false);
    }

    #[test]
    fn anthropic_tool_result_error_flag_preserved() {
        let v = anthropic_messages(&[tool_result("t1", "boom", true)]);
        assert_eq!(v[0]["content"][0]["is_error"], true);
    }

    #[test]
    fn anthropic_full_turn_sequence() {
        let msgs = vec![
            user("run bash"),
            assistant_with_tool(None, "tc1", "bash"),
            tool_result("tc1", "ok", false),
        ];
        let v = anthropic_messages(&msgs);
        assert_eq!(v.as_array().unwrap().len(), 3);
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[1]["role"], "assistant");
        assert_eq!(v[2]["role"], "user"); // tool result → user role
    }

    // ── openai_messages ───────────────────────────────────────────────────────

    #[test]
    fn openai_empty_no_system() {
        let v = openai_messages(&[], "");
        assert_eq!(v, json!([]));
    }

    #[test]
    fn openai_system_injected_first() {
        let v = openai_messages(&[user("hi")], "you are helpful");
        assert_eq!(v[0]["role"], "system");
        assert_eq!(v[0]["content"], "you are helpful");
        assert_eq!(v[1]["role"], "user");
    }

    #[test]
    fn openai_assistant_no_tools_has_content() {
        let v = openai_messages(&[assistant("hello")], "");
        assert_eq!(v[0]["role"], "assistant");
        assert_eq!(v[0]["content"], "hello");
    }

    #[test]
    fn openai_assistant_with_tools_content_is_null() {
        let v = openai_messages(&[assistant_with_tool(None, "tc1", "bash")], "");
        // OpenAI spec: content must be null when tool_calls present
        assert!(
            v[0]["content"].is_null(),
            "content should be null with tool_calls, got: {:?}",
            v[0]["content"]
        );
        assert!(v[0]["tool_calls"].is_array());
    }

    #[test]
    fn openai_tool_call_arguments_serialized_as_string() {
        let v = openai_messages(&[assistant_with_tool(None, "tc1", "bash")], "");
        let args = v[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        // Must be a JSON string, not an object
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["arg"], "val");
    }

    #[test]
    fn openai_tool_results_use_tool_role() {
        let v = openai_messages(&[tool_result("tc1", "done", false)], "");
        assert_eq!(v[0]["role"], "tool");
        assert_eq!(v[0]["tool_call_id"], "tc1");
        assert_eq!(v[0]["content"], "done");
    }

    #[test]
    fn openai_multiple_tool_results_each_get_own_message() {
        let msg = Message::ToolResults(vec![
            ToolResult {
                tool_use_id: "t1".into(),
                content: "a".into(),
                is_error: false,
            },
            ToolResult {
                tool_use_id: "t2".into(),
                content: "b".into(),
                is_error: false,
            },
        ]);
        let v = openai_messages(&[msg], "");
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["tool_call_id"], "t1");
        assert_eq!(v[1]["tool_call_id"], "t2");
    }

    #[test]
    fn openai_user_message_no_system_no_prefix() {
        let v = openai_messages(&[user("test")], "");
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["role"], "user");
    }

    // ── anthropic_tools / openai_tools ────────────────────────────────────────

    fn make_tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: "does stuff".to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
        }
    }

    #[test]
    fn anthropic_tools_empty_list() {
        let v = anthropic_tools(&[]);
        assert_eq!(v, json!([]));
    }

    #[test]
    fn anthropic_tools_shape() {
        let v = anthropic_tools(&[make_tool("bash")]);
        assert_eq!(v[0]["name"], "bash");
        assert_eq!(v[0]["description"], "does stuff");
        assert!(v[0]["input_schema"].is_object());
    }

    #[test]
    fn openai_tools_empty_list() {
        let v = openai_tools(&[]);
        assert_eq!(v, json!([]));
    }

    #[test]
    fn openai_tools_shape() {
        let v = openai_tools(&[make_tool("bash")]);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "bash");
        assert_eq!(v[0]["function"]["description"], "does stuff");
        assert!(v[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn openai_tools_multiple() {
        let tools = vec![make_tool("read_file"), make_tool("write_file")];
        let v = openai_tools(&tools);
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["function"]["name"], "read_file");
        assert_eq!(v[1]["function"]["name"], "write_file");
    }
}
