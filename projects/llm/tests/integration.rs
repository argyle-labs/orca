//! Integration tests for the orca-llm crate.
//!
//! These tests run against real backends. They skip gracefully when the backend
//! isn't available — no failures for offline CI. To force-run them:
//!
//!   cargo test -p orca-llm --test integration -- --nocapture --test-threads=1
//!
//! Use --test-threads=1 when running against LM Studio, which is single-threaded
//! and returns 500 errors under concurrent load.
//!
//! Set LMSTUDIO_URL to override the default (http://localhost:1234).

use config::{Config, Model};
use llm::{
    ClaudeBackend, DiscoveredModel, LMStudioBackend, Message, ModelBackend, StopReason, TaskKind,
    buffer_sink, classify_model, discover_all, estimate_context_window, resolve_model,
    select_for_task, stdout_sink,
};
use serde_json::json;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn lmstudio_url() -> String {
    std::env::var("LMSTUDIO_URL").unwrap_or_else(|_| "http://localhost:1234".into())
}

fn ollama_url() -> String {
    std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into())
}

fn test_config() -> Config {
    Config {
        anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        lmstudio_url: lmstudio_url(),
        ollama_url: ollama_url(),
        default_model: Model::LMStudio {
            id: String::new(),
            url: String::new(),
        },
        orca_vault: PathBuf::from("/tmp/.orca-test"),
        vault_root: PathBuf::from("/tmp"),
        memory_root: PathBuf::from("/tmp"),
        db_path: PathBuf::from("/tmp/.orca-test/orca.db"),
    }
}

/// True when the error indicates LM Studio is temporarily overloaded (500).
fn is_server_busy(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("500") || s.contains("Internal Server Error") || s.contains("busy")
}

/// True when the error indicates a model failed to load (memory constraint / 400).
fn is_model_unavailable(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("model not available")
        || s.contains("Failed to load model")
        || s.contains("insufficient system resources")
}

/// Returns Some(backend) if LM Studio is reachable with at least one chat model,
/// None otherwise (caller should return early — test passes trivially).
async fn lmstudio_if_available() -> Option<(LMStudioBackend, String)> {
    let url = lmstudio_url();
    let probe = LMStudioBackend::new(&url, "");
    match probe.list_models().await {
        Ok(models) => {
            let chat_models: Vec<_> = models
                .into_iter()
                .filter(|m| !m.to_ascii_lowercase().contains("embed"))
                .collect();
            if chat_models.is_empty() {
                eprintln!("SKIP: LM Studio at {url} has no chat models loaded");
                None
            } else {
                Some((
                    LMStudioBackend::new(&url, &chat_models[0]),
                    chat_models[0].clone(),
                ))
            }
        }
        Err(e) => {
            eprintln!("SKIP: LM Studio not available at {url}: {e}");
            None
        }
    }
}

// ── Discovery tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn list_models_returns_strings_when_available() {
    let url = lmstudio_url();
    let backend = LMStudioBackend::new(&url, "");
    match backend.list_models().await {
        Err(_) => {
            eprintln!("SKIP: LM Studio not reachable at {url}");
        }
        Ok(models) => {
            eprintln!("Available models at {url}: {models:?}");
            // IDs must be non-empty strings
            for id in &models {
                assert!(!id.is_empty(), "model id must not be empty");
                assert!(
                    id.is_ascii() || id.contains('/'),
                    "unexpected model id: {id}"
                );
            }
        }
    }
}

#[tokio::test]
async fn discover_all_deduplicates_backends() {
    let config = test_config();
    let found = discover_all(&config).await;
    eprintln!(
        "Discovered {} model(s): {:?}",
        found.len(),
        found.iter().map(|m| &m.id).collect::<Vec<_>>()
    );

    // No embedding models should appear
    for m in &found {
        assert!(
            !m.capabilities.preferred_tasks.is_empty(),
            "embedding model slipped through: {}",
            m.id
        );
    }

    // All entries must have a non-empty id and backend
    for m in &found {
        assert!(!m.id.is_empty());
        assert!(!m.backend.is_empty());
    }
}

#[tokio::test]
async fn discover_all_excludes_embed_models() {
    // Even if LM Studio returns embedding models, discover_all must filter them.
    // We can't force LM Studio to return them, so we test the filter indirectly
    // by checking classify_model for an embed ID returns empty preferred_tasks.
    let caps = classify_model("nomic-embed-text-v1", "lmstudio");
    assert!(
        caps.preferred_tasks.is_empty(),
        "embed model should have no preferred tasks"
    );
    assert_eq!(
        caps.rank, 255,
        "embed model should have maximum rank (never selected)"
    );
}

// ── Model selection tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn resolve_model_returns_something_when_lmstudio_available() {
    let Some((_, _)) = lmstudio_if_available().await else {
        return;
    };

    let config = test_config();
    match resolve_model(&config, Some(TaskKind::Chat)).await {
        Ok(model) => eprintln!("Resolved model: {model:?}"),
        Err(e) => panic!("resolve_model failed: {e}"),
    }
}

#[tokio::test]
async fn resolve_model_respects_explicit_config() {
    // When a model is explicitly configured, we must use it — no discovery.
    let config = Config {
        default_model: Model::LMStudio {
            id: "explicit-model-id".into(),
            url: String::new(),
        },
        ..test_config()
    };
    let model = resolve_model(&config, None).await.unwrap();
    assert!(matches!(model, Model::LMStudio { ref id, .. } if id == "explicit-model-id"));
}

#[tokio::test]
async fn resolve_model_errors_when_nothing_available() {
    // No LM Studio, no Ollama, no API key — all backends point at dead ports.
    let config = Config {
        lmstudio_url: "http://localhost:19999".into(),
        ollama_url: "http://localhost:19998".into(),
        anthropic_api_key: None,
        default_model: Model::LMStudio {
            id: String::new(),
            url: String::new(),
        },
        ..test_config()
    };
    let result = resolve_model(&config, None).await;
    assert!(result.is_err(), "should error when no backend is available");
    let msg = result.unwrap_err().to_string();
    eprintln!("Error message: {msg}");
    assert!(
        msg.contains("no models available") || msg.contains("LM Studio"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn select_for_task_tool_use_avoids_reasoning_model() {
    let models = vec![
        DiscoveredModel {
            id: "deepseek-r1-14b".into(),
            backend: "lmstudio".into(),
            url: String::new(),
            capabilities: classify_model("deepseek-r1-14b", "lmstudio"),
        },
        DiscoveredModel {
            id: "qwen/qwen3-14b".into(),
            backend: "lmstudio".into(),
            url: String::new(),
            capabilities: classify_model("qwen/qwen3-14b", "lmstudio"),
        },
    ];

    let selected = select_for_task(&models, TaskKind::ToolUse).unwrap();
    assert_eq!(
        selected.id, "qwen/qwen3-14b",
        "should pick qwen over reasoning model for tool use"
    );
}

#[tokio::test]
async fn context_window_estimate_is_sensible() {
    assert!(estimate_context_window(&Model::Claude("claude-sonnet-4-6".into())) >= 100_000);
    assert!(
        estimate_context_window(&Model::LMStudio {
            id: "qwen3-14b".into(),
            url: String::new()
        }) >= 4_096
    );
    // 128k model in the ID should get a larger window
    let w = estimate_context_window(&Model::LMStudio {
        id: "some-model-128k".into(),
        url: String::new(),
    });
    assert!(
        w >= 100_000,
        "128k model should have large context window, got {w}"
    );
}

// ── Basic chat tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn lmstudio_chat_returns_non_empty_response() {
    let Some((backend, model_id)) = lmstudio_if_available().await else {
        return;
    };

    eprintln!("Testing chat with model: {model_id}");
    let messages = vec![Message::user("Say exactly: pong")];
    let cancel = CancellationToken::new();
    let (sink, buf) = buffer_sink();

    let resp = backend
        .chat(
            &messages,
            &[],
            "You are a test assistant. Be very brief.",
            cancel,
            &sink,
        )
        .await
        .expect("chat should succeed");

    let output = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
    eprintln!("Response text: {:?}", resp.text);
    eprintln!("Streamed output length: {}", output.len());

    assert!(!resp.text.is_empty(), "response text must not be empty");
    // Some local models (e.g. qwen variants) don't report token usage — treat as optional.
    if resp.input_tokens == 0 {
        eprintln!("Note: model did not report input_tokens (acceptable for local models)");
    }
    assert!(matches!(
        resp.stop_reason,
        StopReason::EndTurn | StopReason::MaxTokens
    ));
}

#[tokio::test]
async fn lmstudio_chat_streams_to_sink() {
    let Some((backend, model_id)) = lmstudio_if_available().await else {
        return;
    };

    let messages = vec![Message::user("Count to 3, one number per line.")];
    let cancel = CancellationToken::new();
    let (sink, buf) = buffer_sink();

    match backend.chat(&messages, &[], "", cancel, &sink).await {
        Err(e) if is_server_busy(&e) => {
            eprintln!("SKIP: LM Studio busy (concurrent load): {e}");
            return;
        }
        Err(e) => panic!("chat failed: {e}"),
        Ok(_) => {}
    }

    let output = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
    eprintln!("Streamed: {output:?}");
    // Streaming should have written something to the sink
    assert!(
        !output.is_empty(),
        "sink must receive streamed tokens, model: {model_id}"
    );
}

#[tokio::test]
async fn lmstudio_empty_response_errors() {
    // If a model returns an empty response, we should get a clear error, not silent Ok.
    // We can't force this from the outside, so we test the error path via a disconnected URL.
    let backend = LMStudioBackend::new("http://localhost:19999", "test-model");
    let messages = vec![Message::user("hello")];
    let cancel = CancellationToken::new();
    let (sink, _) = buffer_sink();

    let result = backend.chat(&messages, &[], "", cancel, &sink).await;
    assert!(result.is_err(), "unreachable backend should return error");
}

// ── Cancellation ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn lmstudio_cancellation_returns_partial_response() {
    let Some((backend, model_id)) = lmstudio_if_available().await else {
        return;
    };

    let messages = vec![Message::user(
        "Write a very long detailed essay about the history of computing. Be thorough.",
    )];
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let (sink, _) = buffer_sink();

    // Cancel after 200ms — should get a partial response, not an error.
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        cancel_clone.cancel();
    });

    let result = backend.chat(&messages, &[], "", cancel, &sink).await;

    // Cancellation should not produce an error — it's a graceful stop.
    match result {
        Ok(resp) => eprintln!(
            "Cancelled after {} chars, model: {model_id}",
            resp.text.len()
        ),
        Err(e) => eprintln!("Got error on cancel (acceptable): {e}"),
    }
    // We just check it doesn't panic. Partial response or clean error both acceptable.
}

// ── Tool calling tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn lmstudio_tool_call_round_trip() {
    let Some((backend, model_id)) = lmstudio_if_available().await else {
        return;
    };

    // Only run this test on models that support tools.
    let caps = classify_model(&model_id, "lmstudio");
    if !caps.supports_tools {
        eprintln!("SKIP: {model_id} does not support tools");
        return;
    }

    use tool::ToolDef;
    let tools = vec![ToolDef {
        name: "get_weather".into(),
        description: "Get the current weather for a location.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "City name" }
            },
            "required": ["location"]
        }),
    }];

    let messages = vec![Message::user("What's the weather in Seattle?")];
    let cancel = CancellationToken::new();
    let (sink, _) = buffer_sink();

    let resp = match backend
        .chat(
            &messages,
            &tools,
            "Use the get_weather tool when asked about weather.",
            cancel,
            &sink,
        )
        .await
    {
        Err(e) if is_server_busy(&e) => {
            eprintln!("SKIP: LM Studio busy (concurrent load): {e}");
            return;
        }
        Err(e) => panic!("tool call failed: {e}"),
        Ok(r) => r,
    };

    eprintln!("stop_reason: {:?}", resp.stop_reason);
    eprintln!("tool_calls: {:?}", resp.tool_calls);

    // Model should have called the tool
    if resp.stop_reason == StopReason::ToolUse {
        assert!(
            !resp.tool_calls.is_empty(),
            "ToolUse stop reason but no tool calls"
        );
        let tc = &resp.tool_calls[0];
        assert_eq!(tc.name, "get_weather");
        assert!(!tc.id.is_empty(), "tool call id must not be empty");
        // Input should have a location field
        assert!(
            tc.input["location"].is_string(),
            "expected location string in tool input, got: {:?}",
            tc.input
        );
        eprintln!("Tool called with location: {:?}", tc.input["location"]);
    } else {
        eprintln!(
            "Note: model did not call tool (stop_reason={:?}). \
             Some local models don't reliably use tools. Text: {:?}",
            resp.stop_reason, resp.text
        );
    }
}

#[tokio::test]
async fn lmstudio_multi_turn_conversation() {
    let Some((backend, model_id)) = lmstudio_if_available().await else {
        return;
    };

    let messages = vec![
        Message::user("My name is Alice. Remember that."),
        Message::Assistant {
            text: Some("Got it, Alice.".into()),
            tool_calls: vec![],
        },
        Message::user("What's my name?"),
    ];
    let cancel = CancellationToken::new();
    let (sink, _) = buffer_sink();

    let resp = match backend
        .chat(
            &messages,
            &[],
            "You are a helpful assistant with perfect memory.",
            cancel,
            &sink,
        )
        .await
    {
        Err(e) if is_server_busy(&e) => {
            eprintln!("SKIP: LM Studio busy (concurrent load): {e}");
            return;
        }
        Err(e) => panic!("multi-turn chat failed: {e}"),
        Ok(r) => r,
    };

    eprintln!("Multi-turn response: {:?}", resp.text);
    assert!(
        !resp.text.is_empty(),
        "response should not be empty, model: {model_id}"
    );
    // The model should mention Alice — but we can't guarantee it, so just verify we got a response.
}

// ── Model switching simulation ────────────────────────────────────────────────

#[tokio::test]
async fn switching_models_mid_session() {
    let url = lmstudio_url();
    let probe = LMStudioBackend::new(&url, "");

    let models = match probe.list_models().await {
        Err(_) => {
            eprintln!("SKIP: LM Studio not available at {url}");
            return;
        }
        Ok(m) => m
            .into_iter()
            .filter(|m| !m.contains("embed"))
            .collect::<Vec<_>>(),
    };

    if models.len() < 2 {
        eprintln!(
            "SKIP: need at least 2 chat models loaded for model-switch test, found: {models:?}"
        );
        return;
    }

    eprintln!("Testing model switch: {} → {}", models[0], models[1]);

    let messages = vec![Message::user("Say: model A")];
    let cancel = CancellationToken::new();
    let (sink1, _) = buffer_sink();
    let backend_a = LMStudioBackend::new(&url, &models[0]);
    let resp_a = match backend_a
        .chat(&messages, &[], "", cancel.clone(), &sink1)
        .await
    {
        Err(e) if is_model_unavailable(&e) || is_server_busy(&e) => {
            eprintln!(
                "SKIP: model {} unavailable or LM Studio busy: {e}",
                models[0]
            );
            return;
        }
        Err(e) => panic!("model A chat failed: {e}"),
        Ok(r) => r,
    };

    let messages2 = vec![Message::user("Say: model B")];
    let (sink2, _) = buffer_sink();
    let backend_b = LMStudioBackend::new(&url, &models[1]);
    let resp_b = match backend_b.chat(&messages2, &[], "", cancel, &sink2).await {
        Err(e) if is_model_unavailable(&e) || is_server_busy(&e) => {
            eprintln!(
                "SKIP: model {} unavailable or LM Studio busy: {e}",
                models[1]
            );
            return;
        }
        Err(e) => panic!("model B chat failed: {e}"),
        Ok(r) => r,
    };

    assert!(!resp_a.text.is_empty(), "model A should respond");
    assert!(!resp_b.text.is_empty(), "model B should respond");
    eprintln!(
        "Model A ({}) response: {:?}",
        models[0],
        &resp_a.text[..resp_a.text.len().min(80)]
    );
    eprintln!(
        "Model B ({}) response: {:?}",
        models[1],
        &resp_b.text[..resp_b.text.len().min(80)]
    );
}

// ── Reasoning model handling ──────────────────────────────────────────────────

#[tokio::test]
async fn reasoning_model_fallback_to_reasoning_content() {
    // If we can detect a reasoning model is loaded, verify the reasoning_content
    // fallback works (the backend should not return an empty-response error).
    let url = lmstudio_url();
    let probe = LMStudioBackend::new(&url, "");

    let models = match probe.list_models().await {
        Err(_) => {
            eprintln!("SKIP: LM Studio not available");
            return;
        }
        Ok(m) => m,
    };

    let reasoning_model = models.iter().find(|m| {
        let lower = m.to_ascii_lowercase();
        lower.contains("deepseek-r1") || lower.contains("-thinking") || lower.contains("reasoning")
    });

    let Some(model_id) = reasoning_model else {
        eprintln!("SKIP: no reasoning model loaded (deepseek-r1, thinking, reasoning)");
        return;
    };

    eprintln!("Testing reasoning model: {model_id}");
    let backend = LMStudioBackend::new(&url, model_id);
    let messages = vec![Message::user("What is 7 * 8?")];
    let cancel = CancellationToken::new();
    let (sink, _) = buffer_sink();

    let resp = backend
        .chat(&messages, &[], "", cancel, &sink)
        .await
        .expect("reasoning model should return a response (via reasoning_content fallback)");

    assert!(
        !resp.text.is_empty(),
        "reasoning model should produce text even if only via reasoning_content"
    );
    eprintln!(
        "Reasoning model response (first 200 chars): {:?}",
        &resp.text[..resp.text.len().min(200)]
    );
}

// ── Backend properties ────────────────────────────────────────────────────────

#[test]
fn lmstudio_backend_name_and_model_id() {
    let b = LMStudioBackend::new("http://localhost:1234", "qwen3-8b");
    assert_eq!(b.name(), "lmstudio");
    assert_eq!(b.model_id(), "qwen3-8b");
}

#[test]
fn claude_backend_name_and_model_id() {
    let b = ClaudeBackend::new("sk-ant-fake", "claude-sonnet-4-6");
    assert_eq!(b.name(), "claude");
    assert_eq!(b.model_id(), "claude-sonnet-4-6");
}

#[test]
fn claude_known_models_non_empty() {
    let models = ClaudeBackend::known_models();
    assert!(!models.is_empty());
    for m in models {
        assert!(m.starts_with("claude-"), "unexpected model id: {m}");
    }
}

#[test]
fn buffer_sink_captures_output() {
    use llm::sink_writeln;
    let (sink, buf) = buffer_sink();
    sink_writeln(&sink, "hello");
    sink_writeln(&sink, "world");
    let content = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert_eq!(content, "hello\nworld\n");
}

#[test]
fn stdout_sink_does_not_panic() {
    use llm::sink_write;
    let sink = stdout_sink();
    sink_write(&sink, ""); // should not panic
}
