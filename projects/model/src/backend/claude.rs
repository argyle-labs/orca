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
