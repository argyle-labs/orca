use crate::types::{Message};
use tool::ToolDef;
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
