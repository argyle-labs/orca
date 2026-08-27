// Anthropic Claude provider SDK request/response envelopes; HashMap/Value are wire-format passthrough.
#![allow(clippy::disallowed_types)]
use super::{BoxFuture, ModelBackend, OutputSink, serialize, sink_write, sink_writeln};
use crate::types::{BackendResponse, Message, StopReason};
use anyhow::{Context, Result, bail};
use colored::Colorize;
use contract::{ToolCall, ToolDef};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use utils::http::{Client, StreamResponse};

pub struct ClaudeBackend {
    client: Client,
    api_key: String,
    model: String,
}

impl ClaudeBackend {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        crate::ensure_crypto_provider();
        ClaudeBackend {
            client: Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Anthropic models available for server-side use, newest-first.
    pub fn known_models() -> &'static [&'static str] {
        &[
            "claude-sonnet-4-6",
            "claude-opus-4-7",
            "claude-haiku-4-5-20251001",
        ]
    }
}

impl ModelBackend for ClaudeBackend {
    fn name(&self) -> &str {
        "claude"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn is_local(&self) -> bool {
        false
    }

    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        system: &'a str,
        cancel: CancellationToken,
        output: &'a OutputSink,
    ) -> BoxFuture<'a, Result<BackendResponse>> {
        Box::pin(async move {
            let claude_messages = serialize::anthropic_messages(messages);

            let mut body = json!({
                "model": self.model,
                "max_tokens": 8192,
                "system": system,
                "messages": claude_messages,
                "stream": true,
            });

            if !tools.is_empty() {
                body["tools"] = serialize::anthropic_tools(tools);
            }

            let response = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send_stream()
                .await
                .context("failed to connect to Anthropic API")?;

            if !response.is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("Anthropic API error {status}: {text}");
            }

            parse_claude_stream(response, cancel, output).await
        })
    }
}

async fn parse_claude_stream(
    response: StreamResponse,
    cancel: CancellationToken,
    output: &OutputSink,
) -> Result<BackendResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    let mut result = BackendResponse::default();

    // Per-index state for streaming content blocks
    // true = tool_use block, false = text block
    let mut block_types: HashMap<usize, bool> = HashMap::new();
    // Accumulated tool use data per block index
    let mut tool_accum: HashMap<usize, (String, String, String)> = HashMap::new(); // (id, name, json)

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                sink_writeln(output, &format!("{}", "\n[interrupted]".yellow()));
                break;
            }
            chunk = stream.next() => {
                match chunk {
                    Some(c) => c,
                    None => break,
                }
            }
        };
        let chunk = chunk.context("stream error")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete lines
        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim().to_string();
            buffer.drain(..=pos);

            if !line.starts_with("data: ") {
                continue;
            }
            let data = &line[6..];
            if data == "[DONE]" || data.is_empty() {
                continue;
            }

            let event: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match event["type"].as_str().unwrap_or("") {
                "message_start" => {
                    if let Some(usage) = event["message"]["usage"].as_object() {
                        result.input_tokens = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                    }
                }

                "content_block_start" => {
                    let idx = event["index"].as_u64().unwrap_or(0) as usize;
                    let block_type = event["content_block"]["type"].as_str().unwrap_or("");
                    match block_type {
                        "tool_use" => {
                            block_types.insert(idx, true);
                            let id = event["content_block"]["id"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            let name = event["content_block"]["name"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            sink_write(output, &format!("\n{}", format!("⚙ {name}").cyan()));
                            tool_accum.insert(idx, (id, name, String::new()));
                        }
                        "text" => {
                            block_types.insert(idx, false);
                        }
                        _ => {}
                    }
                }

                "content_block_delta" => {
                    let idx = event["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &event["delta"];

                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            if let Some(text) = delta["text"].as_str() {
                                sink_write(output, text);
                                result.text.push_str(text);
                            }
                        }
                        "input_json_delta" => {
                            if let (Some(partial), Some(entry)) =
                                (delta["partial_json"].as_str(), tool_accum.get_mut(&idx))
                            {
                                entry.2.push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }

                "content_block_stop" => {
                    let idx = event["index"].as_u64().unwrap_or(0) as usize;
                    if block_types.get(&idx) == Some(&true)
                        && let Some((id, name, json_str)) = tool_accum.remove(&idx)
                    {
                        let input: Value = serde_json::from_str(&json_str).unwrap_or(json!({}));
                        result.tool_calls.push(ToolCall { id, name, input });
                    }
                }

                "message_delta" => {
                    if let Some(usage) = event["usage"].as_object() {
                        result.output_tokens = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                    }
                    result.stop_reason = match event["delta"]["stop_reason"].as_str() {
                        Some("tool_use") => StopReason::ToolUse,
                        Some("max_tokens") => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    };
                }

                _ => {}
            }
        }
    }

    if !result.text.is_empty() || !result.tool_calls.is_empty() {
        sink_writeln(output, ""); // newline after streamed content
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::buffer_sink;
    use std::io::{Read, Write as _};
    use std::net::TcpListener;

    /// Spawn a one-shot HTTP/1.1 server on 127.0.0.1 that replies to the first
    /// connection with `body` as the response body, then returns its URL.
    fn serve_once(body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read request headers (until CRLFCRLF) so the client isn't reset.
                let mut buf = [0u8; 1024];
                _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                _ = stream.write_all(resp.as_bytes());
                _ = stream.flush();
            }
        });
        format!("http://{addr}/v1/messages")
    }

    /// Format a list of JSON events into an SSE body.
    fn sse(events: &[Value]) -> String {
        let mut s = String::new();
        for e in events {
            s.push_str("data: ");
            s.push_str(&serde_json::to_string(e).unwrap());
            s.push_str("\n\n");
        }
        s
    }

    async fn run_stream(body: String, cancel: CancellationToken) -> BackendResponse {
        let url = serve_once(body);
        let (sink, _buf) = buffer_sink();
        let resp = Client::new()
            .post(url)
            .send_stream()
            .await
            .expect("connect to test server");
        assert!(resp.is_success());
        parse_claude_stream(resp, cancel, &sink).await.unwrap()
    }

    #[test]
    fn backend_metadata_and_known_models() {
        let b = ClaudeBackend::new("sk-test", "claude-sonnet-4-6");
        assert_eq!(b.name(), "claude");
        assert_eq!(b.model_id(), "claude-sonnet-4-6");
        assert!(!b.is_local());
        assert!(b.supports_tools());

        let models = ClaudeBackend::known_models();
        assert_eq!(models[0], "claude-sonnet-4-6");
        assert!(models.contains(&"claude-opus-4-7"));
    }

    #[tokio::test]
    async fn parses_text_and_tool_use_stream() {
        let events = vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 42}}}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Hello"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": " world"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "t1", "name": "get_weather"}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "\"SF\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "usage": {"output_tokens": 7},
                   "delta": {"stop_reason": "tool_use"}}),
        ];
        let mut body = sse(&events);
        // Exercise the ignored-line branches: non-data line, [DONE], empty data,
        // malformed JSON, and an unknown event type.
        body.push_str(": comment line\n\n");
        body.push_str("data: [DONE]\n\n");
        body.push_str("data: \n\n");
        body.push_str("data: {not json}\n\n");
        body.push_str("data: {\"type\":\"ping\"}\n\n");

        let r = run_stream(body, CancellationToken::new()).await;
        assert_eq!(r.text, "Hello world");
        assert_eq!(r.input_tokens, 42);
        assert_eq!(r.output_tokens, 7);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "t1");
        assert_eq!(r.tool_calls[0].name, "get_weather");
        assert_eq!(r.tool_calls[0].input, json!({"city": "SF"}));
    }

    #[tokio::test]
    async fn max_tokens_stop_reason_and_empty_tool_json_defaults() {
        let events = vec![
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": "t9", "name": "noargs"}}),
            // no input_json_delta at all -> accumulated json is empty -> defaults to {}
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
        ];
        let r = run_stream(sse(&events), CancellationToken::new()).await;
        assert_eq!(r.stop_reason, StopReason::MaxTokens);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].input, json!({}));
        assert!(r.text.is_empty());
    }

    #[tokio::test]
    async fn end_turn_is_the_default_stop_reason() {
        let events = vec![
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "done"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ];
        let r = run_stream(sse(&events), CancellationToken::new()).await;
        assert_eq!(r.text, "done");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert!(r.tool_calls.is_empty());
    }
}
