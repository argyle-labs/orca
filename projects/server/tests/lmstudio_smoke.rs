#![allow(clippy::disallowed_types)]
// test harness — Value used for flexible response assertions
// Serializes concurrent test execution with a std::Mutex held across await — intentional.
#![allow(clippy::await_holding_lock)]

/// LM Studio smoke tests — validate end-to-end communication with local models.
///
/// Each test owns the model lifecycle: unload everything, load its model, run,
/// unload everything again.  Tests are `#[ignore]` so they never run in CI;
/// invoke individually:
///
///   cargo test -p orca lmstudio_ -- --ignored --nocapture
///
/// Requirements: LM Studio running on localhost:1234, `lms` CLI on PATH,
/// and the models referenced below available on disk.
use ::model::backend::{LMStudioBackend, ModelBackend, buffer_sink};
use ::model::{Message, StopReason};
use contract::ToolDef;
use serde_json::json;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tokio_util::sync::CancellationToken;

// Serialize all live tests within a single test-binary run — prevents concurrent
// load/unload calls when multiple test names are matched by a broad filter.
static LMS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn acquire_lms() -> MutexGuard<'static, ()> {
    LMS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// ── Model identifiers ─────────────────────────────────────────────────────────

const FAST_MODEL: &str = "google/gemma-4-e4b";
const TOOL_MODEL: &str = "qwen/qwen3.5-9b";
const THINK_MODEL: &str = "deepseek/deepseek-r1-0528-qwen3-8b";
const LMS_URL: &str = "http://localhost:1234";

// ── Lifecycle helpers ─────────────────────────────────────────────────────────

fn unload_all() {
    let status = Command::new("lms")
        .args(["unload", "--all"])
        .status()
        .expect("lms unload --all failed");
    assert!(status.success(), "lms unload --all exited non-zero");
}

fn load_model(id: &str) {
    let status = Command::new("lms")
        .args(["load", id, "--context-length", "4096"])
        .status()
        .expect("lms load failed");
    assert!(status.success(), "lms load {id} exited non-zero");
}

fn setup(model_id: &str) -> LMStudioBackend {
    unload_all();
    load_model(model_id);
    LMStudioBackend::new(LMS_URL, model_id)
}

fn teardown() {
    unload_all();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Basic round-trip: send a simple question, expect a non-empty text response.
#[tokio::test]
#[ignore]
async fn lmstudio_basic_chat() {
    let _lock = acquire_lms();
    let backend = setup(FAST_MODEL);
    let (sink, buf) = buffer_sink();

    let messages = vec![Message::user("Reply with exactly the word: pong")];
    let response = backend
        .chat(
            &messages,
            &[],
            "You are a helpful assistant.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("chat failed");

    teardown();

    let output = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert!(
        !response.text.is_empty(),
        "expected non-empty text response; output was: {output:?}"
    );
    assert_eq!(
        response.stop_reason,
        StopReason::EndTurn,
        "expected EndTurn stop reason"
    );
    // Streamed text should have landed in the sink too
    assert!(!output.is_empty(), "nothing was written to output sink");
}

/// Multi-turn: second message should reference context from the first.
#[tokio::test]
#[ignore]
async fn lmstudio_multi_turn_context() {
    let _lock = acquire_lms();
    let backend = setup(FAST_MODEL);
    let (sink, _buf) = buffer_sink();

    let messages = vec![
        Message::user("My favourite colour is ultraviolet. Remember it."),
        Message::Assistant {
            text: Some("Noted, your favourite colour is ultraviolet.".into()),
            tool_calls: vec![],
        },
        Message::user("What is my favourite colour?"),
    ];

    let response = backend
        .chat(
            &messages,
            &[],
            "You are a helpful assistant.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("chat failed");

    teardown();

    let text = response.text.to_lowercase();
    assert!(
        text.contains("ultraviolet"),
        "model forgot context; got: {text:?}"
    );
}

/// Tool call: model should call the provided tool and the stop reason should
/// be ToolUse with a populated tool_calls list.
#[tokio::test]
#[ignore]
async fn lmstudio_tool_call() {
    let _lock = acquire_lms();
    let backend = setup(TOOL_MODEL);
    let (sink, _buf) = buffer_sink();

    let tools = vec![ToolDef {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "city": { "type": "string", "description": "City name" }
            },
            "required": ["city"]
        }),
    }];

    let messages = vec![Message::user(
        "What is the weather in Dublin? Use the get_weather tool.",
    )];

    let response = backend
        .chat(
            &messages,
            &tools,
            "You are a helpful assistant. Always use tools when available.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("chat failed");

    teardown();

    assert!(
        !response.tool_calls.is_empty(),
        "expected at least one tool call; got none. text: {:?}",
        response.text
    );
    let tc = &response.tool_calls[0];
    assert_eq!(tc.name, "get_weather", "wrong tool called: {}", tc.name);
    assert!(
        tc.input["city"].as_str().is_some(),
        "expected city arg; got: {:?}",
        tc.input
    );
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

/// Tool call + result → follow-up: model receives tool result and produces a
/// final text answer without requesting more tools.
#[tokio::test]
#[ignore]
async fn lmstudio_tool_call_then_answer() {
    let _lock = acquire_lms();
    let backend = setup(TOOL_MODEL);
    let (sink, _buf) = buffer_sink();

    let tools = vec![ToolDef {
        name: "lookup_capital".into(),
        description: "Look up the capital city of a country.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "country": { "type": "string" }
            },
            "required": ["country"]
        }),
    }];

    // Round 1: model calls tool
    let round1_messages = vec![Message::user(
        "What is the capital of France? Use the lookup_capital tool.",
    )];
    let r1 = backend
        .chat(
            &round1_messages,
            &tools,
            "You are a helpful assistant. Always use tools when available.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("round 1 failed");

    assert!(!r1.tool_calls.is_empty(), "expected tool call in round 1");
    let tc = &r1.tool_calls[0];

    // Round 2: provide tool result, expect final text answer
    use contract::ToolResult;
    let round2_messages = vec![
        Message::user("What is the capital of France? Use the lookup_capital tool."),
        Message::Assistant {
            text: if r1.text.is_empty() {
                None
            } else {
                Some(r1.text.clone())
            },
            tool_calls: r1.tool_calls.clone(),
        },
        Message::ToolResults(vec![ToolResult {
            tool_use_id: tc.id.clone(),
            content: "Paris".into(),
            is_error: false,
        }]),
    ];

    let (sink2, _buf2) = buffer_sink();
    let r2 = backend
        .chat(
            &round2_messages,
            &tools,
            "You are a helpful assistant. Always use tools when available.",
            CancellationToken::new(),
            &sink2,
        )
        .await
        .expect("round 2 failed");

    teardown();

    let text = r2.text.to_lowercase();
    assert!(
        text.contains("paris"),
        "expected 'paris' in final answer; got: {text:?}"
    );
    assert_eq!(r2.stop_reason, StopReason::EndTurn);
}

/// Thinking model: deepseek-r1 / qwen3 with reasoning emits reasoning_content
/// before the answer. The text field should contain only the visible answer,
/// and it should be non-empty.
#[tokio::test]
#[ignore]
async fn lmstudio_thinking_model() {
    let _lock = acquire_lms();
    let backend = setup(THINK_MODEL);
    let (sink, buf) = buffer_sink();

    let messages = vec![Message::user("What is 7 * 8? Show only the number.")];

    let response = backend
        .chat(
            &messages,
            &[],
            "You are a helpful assistant.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("chat failed");

    teardown();

    let text = response.text.trim().to_string();
    assert!(
        !text.is_empty(),
        "expected non-empty answer from thinking model"
    );
    assert!(
        text.contains("56"),
        "expected '56' in answer; got: {text:?}"
    );

    // Sink output may include dimmed thinking content — just verify something arrived
    let output = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert!(!output.is_empty(), "nothing written to output sink");
}

/// Cancellation: cancel the token mid-stream; should return partial (possibly
/// empty) result without error, and the interrupt marker should appear in output.
#[tokio::test]
#[ignore]
async fn lmstudio_cancellation() {
    let _lock = acquire_lms();
    let backend = setup(FAST_MODEL);
    let (sink, buf) = buffer_sink();
    let cancel = CancellationToken::new();

    let messages = vec![Message::user("Count from 1 to 1000, one number per line.")];

    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        cancel_clone.cancel();
    });

    // Should not error — partial result is returned
    let result = backend
        .chat(
            &messages,
            &[],
            "You are a helpful assistant.",
            cancel,
            &sink,
        )
        .await;

    teardown();

    // The stream might have been empty if cancel fired before the first chunk.
    // Either way: no panic, no error, and the interrupt marker appears in sink.
    match result {
        Ok(_) => {}
        Err(e) => panic!("cancellation should not propagate as error; got: {e}"),
    }
    let output = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert!(
        output.contains("[interrupted]"),
        "expected '[interrupted]' in sink output; got: {output:?}"
    );
}

/// Serialize round-trip: openai_messages with tool calls must emit null content,
/// not an empty string. This validates the fix for Ollama / llama.cpp compat.
#[test]
fn serialize_tool_call_content_is_null() {
    use ::model::Message;
    use ::model::backend::serialize::openai_messages;
    use contract::ToolCall;
    use serde_json::Value;

    let messages = vec![Message::Assistant {
        text: None,
        tool_calls: vec![ToolCall {
            id: "call_1".into(),
            name: "my_tool".into(),
            input: json!({"x": 1}),
        }],
    }];

    let serialized = openai_messages(&messages, "");
    let arr = serialized.as_array().expect("expected array");
    let msg = &arr[0];

    // content must be null, not ""
    assert_eq!(
        msg["content"],
        Value::Null,
        "expected null content alongside tool_calls; got: {:?}",
        msg["content"]
    );
}

/// Serialize: assistant message with text AND tool_calls emits the text as content.
#[test]
fn serialize_tool_call_with_text_keeps_content() {
    use ::model::Message;
    use ::model::backend::serialize::openai_messages;
    use contract::ToolCall;

    let messages = vec![Message::Assistant {
        text: Some("Thinking…".into()),
        tool_calls: vec![ToolCall {
            id: "call_2".into(),
            name: "my_tool".into(),
            input: json!({}),
        }],
    }];

    let serialized = openai_messages(&messages, "");
    let msg = &serialized.as_array().unwrap()[0];
    assert_eq!(msg["content"], "Thinking…");
}

/// Serialize: system prompt appears as first message when non-empty.
#[test]
fn serialize_system_prompt_prepended() {
    use ::model::backend::serialize::openai_messages;

    let messages = vec![Message::User {
        content: "hello".into(),
    }];
    let serialized = openai_messages(&messages, "You are a robot.");
    let arr = serialized.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[0]["content"], "You are a robot.");
    assert_eq!(arr[1]["role"], "user");
}

/// Serialize: empty system prompt is NOT prepended.
#[test]
fn serialize_empty_system_prompt_omitted() {
    use ::model::backend::serialize::openai_messages;

    let messages = vec![Message::User {
        content: "hello".into(),
    }];
    let serialized = openai_messages(&messages, "");
    let arr = serialized.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["role"], "user");
}

/// Serialize: tool results become role=tool with correct tool_call_id.
#[test]
fn serialize_tool_results_role_and_id() {
    use ::model::backend::serialize::openai_messages;
    use contract::ToolResult;

    let messages = vec![Message::ToolResults(vec![
        ToolResult {
            tool_use_id: "call_abc".into(),
            content: "the answer is 42".into(),
            is_error: false,
        },
        ToolResult {
            tool_use_id: "call_def".into(),
            content: "some error".into(),
            is_error: true,
        },
    ])];

    let serialized = openai_messages(&messages, "");
    let arr = serialized.as_array().unwrap();
    assert_eq!(arr.len(), 2, "each ToolResult becomes its own message");
    assert_eq!(arr[0]["role"], "tool");
    assert_eq!(arr[0]["tool_call_id"], "call_abc");
    assert_eq!(arr[0]["content"], "the answer is 42");
    assert_eq!(arr[1]["tool_call_id"], "call_def");
    assert_eq!(arr[1]["content"], "some error");
}

/// Serialize: assistant with no text and no tool_calls emits empty string content.
#[test]
fn serialize_assistant_empty_is_empty_string() {
    use ::model::backend::serialize::openai_messages;

    let messages = vec![Message::Assistant {
        text: None,
        tool_calls: vec![],
    }];

    let serialized = openai_messages(&messages, "");
    let arr = serialized.as_array().unwrap();
    assert_eq!(arr[0]["role"], "assistant");
    assert_eq!(arr[0]["content"], "");
}

/// Serialize: full conversation round-trip order is preserved.
#[test]
fn serialize_conversation_order() {
    use ::model::backend::serialize::openai_messages;
    use contract::{ToolCall, ToolResult};

    let messages = vec![
        Message::User {
            content: "question".into(),
        },
        Message::Assistant {
            text: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "lookup".into(),
                input: json!({"q": "x"}),
            }],
        },
        Message::ToolResults(vec![ToolResult {
            tool_use_id: "c1".into(),
            content: "result".into(),
            is_error: false,
        }]),
        Message::Assistant {
            text: Some("final answer".into()),
            tool_calls: vec![],
        },
    ];

    let serialized = openai_messages(&messages, "sys");
    let arr = serialized.as_array().unwrap();
    // system, user, assistant(tool_call), tool, assistant(text)
    assert_eq!(arr.len(), 5);
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[1]["role"], "user");
    assert_eq!(arr[2]["role"], "assistant");
    assert!(arr[2]["tool_calls"].is_array());
    assert_eq!(arr[3]["role"], "tool");
    assert_eq!(arr[4]["role"], "assistant");
    assert_eq!(arr[4]["content"], "final answer");
}

// ── Edge case live tests ──────────────────────────────────────────────────────

/// System prompt is respected: model should follow the constraint in the system prompt.
#[tokio::test]
#[ignore]
async fn lmstudio_system_prompt_respected() {
    let _lock = acquire_lms();
    let backend = setup(FAST_MODEL);
    let (sink, _buf) = buffer_sink();

    let messages = vec![Message::user("What is 2 + 2?")];

    let response = backend
        .chat(
            &messages,
            &[],
            // System prompt constrains the response format tightly
            "You are a calculator. Respond with ONLY a single integer. No other text.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("chat failed");

    teardown();

    let text = response.text.trim().to_string();
    assert!(
        text.contains("4"),
        "expected '4' in response; got: {text:?}"
    );
}

/// Tool error: model receives a tool result with is_error=true and should
/// acknowledge the failure rather than hallucinating a success.
#[tokio::test]
#[ignore]
async fn lmstudio_tool_error_handled() {
    let _lock = acquire_lms();
    let backend = setup(TOOL_MODEL);
    let (sink, _buf) = buffer_sink();

    let tools = vec![ToolDef {
        name: "fetch_data".into(),
        description: "Fetch data from a remote service.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" }
            },
            "required": ["url"]
        }),
    }];

    // Round 1: get a tool call
    let r1_messages = vec![Message::user(
        "Fetch data from http://example.com and tell me what it says.",
    )];
    let r1 = backend
        .chat(
            &r1_messages,
            &tools,
            "You are a helpful assistant. Use tools when available.",
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("round 1 failed");

    // If model didn't call the tool, skip the error-handling part
    if r1.tool_calls.is_empty() {
        teardown();
        return;
    }

    let tc = &r1.tool_calls[0];
    use contract::ToolResult;

    // Round 2: return an error result
    let (sink2, _buf2) = buffer_sink();
    let r2_messages = vec![
        Message::user("Fetch data from http://example.com and tell me what it says."),
        Message::Assistant {
            text: if r1.text.is_empty() {
                None
            } else {
                Some(r1.text.clone())
            },
            tool_calls: r1.tool_calls.clone(),
        },
        Message::ToolResults(vec![ToolResult {
            tool_use_id: tc.id.clone(),
            content: "Error: connection refused".into(),
            is_error: true,
        }]),
    ];

    let r2 = backend
        .chat(
            &r2_messages,
            &tools,
            "You are a helpful assistant. Use tools when available.",
            CancellationToken::new(),
            &sink2,
        )
        .await
        .expect("round 2 failed");

    teardown();

    // Model should produce a text response acknowledging the error
    assert!(
        !r2.text.is_empty(),
        "expected model to respond to tool error; got empty text"
    );
    assert_eq!(
        r2.stop_reason,
        StopReason::EndTurn,
        "expected EndTurn after error result"
    );
}

/// Orca MCP `run_agent` dispatches a one-shot prompt through the local LLM.
/// Validates the full path: Session → resolve_model → LMStudioBackend → one_shot.
/// The model used is whichever LM Studio has loaded (auto-selected by resolve_model).
#[tokio::test]
#[ignore]
async fn lmstudio_mcp_run_agent_offload() {
    let _lock = acquire_lms();
    unload_all();
    load_model(FAST_MODEL);

    // Build a Config that points at LM Studio and has no Anthropic key — ensures
    // build_backend can only produce an LMStudioBackend.
    let home = std::env::var("HOME").expect("HOME not set");
    let config = contract::config::Config {
        anthropic_api_key: None,
        lmstudio_url: LMS_URL.to_string(),
        ollama_url: String::new(),
        default_model: contract::config::Model::LMStudio {
            id: FAST_MODEL.to_string(),
            url: String::new(),
        },
        app_dir: std::path::PathBuf::from(format!("{home}/.orca")),
        memory_root: std::path::PathBuf::from(format!("{home}/.orca/memory")),
        db_path: std::path::PathBuf::from(format!("{home}/.orca/orca.db")),
        ports: Default::default(),
    };

    let (sink, buf) = buffer_sink();
    let ctx = conversation::sessions::context::ProjectContext::default();
    let mut session = conversation::sessions::session::Session::new_with_output(config, ctx, sink)
        .await
        .expect("failed to create session");

    // Run a one-shot task through the full session pipeline
    session
        .one_shot("Reply with exactly the number 42 and nothing else.".to_string())
        .await
        .expect("one_shot failed");

    teardown();

    let output = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    eprintln!("run_agent output: {output:?}");
    assert!(
        !output.is_empty(),
        "expected non-empty output from one_shot via local LLM"
    );
    assert!(
        output.contains("42"),
        "expected '42' in output; got: {output:?}"
    );
}

/// Concurrent context windows: two independent backends hitting the same model
/// should each return correct, independent responses.
#[tokio::test]
#[ignore]
async fn lmstudio_independent_contexts() {
    let _lock = acquire_lms();
    let backend = setup(FAST_MODEL);

    let (sink_a, _buf_a) = buffer_sink();
    let (sink_b, _buf_b) = buffer_sink();

    let msg_a = vec![Message::user("My secret word is ALPHA. What is it?")];
    let msg_b = vec![Message::user("My secret word is BETA. What is it?")];

    let (ra, rb) = tokio::join!(
        backend.chat(
            &msg_a,
            &[],
            "You are a helpful assistant.",
            CancellationToken::new(),
            &sink_a
        ),
        backend.chat(
            &msg_b,
            &[],
            "You are a helpful assistant.",
            CancellationToken::new(),
            &sink_b
        ),
    );

    teardown();

    let text_a = ra.expect("request A failed").text.to_lowercase();
    let text_b = rb.expect("request B failed").text.to_lowercase();

    assert!(
        text_a.contains("alpha"),
        "A should contain ALPHA; got: {text_a:?}"
    );
    assert!(
        text_b.contains("beta"),
        "B should contain BETA; got: {text_b:?}"
    );
    // Cross-contamination check
    assert!(
        !text_a.contains("beta"),
        "A should NOT contain BETA; got: {text_a:?}"
    );
    assert!(
        !text_b.contains("alpha"),
        "B should NOT contain ALPHA; got: {text_b:?}"
    );
}
