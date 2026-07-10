// LM Studio provider SDK request/response envelopes; HashMap/Value are wire-format passthrough.
#![allow(clippy::disallowed_types)]
use super::{BoxFuture, ModelBackend, OutputSink, serialize, sink_write, sink_writeln};
use crate::types::{BackendResponse, Message, StopReason};
use anyhow::{Context, Result, bail};
use colored::Colorize;
use contract::{ToolCall, ToolDef};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use utils::http::{Client, StreamResponse};

/// Bail if no streamed chunk arrives for this long. Reasoning models can sit
/// silently producing internal tokens with no visible output; this keeps the
/// CLI from hanging forever when the server is genuinely stuck (model crashed,
/// runtime deadlocked) without penalizing legitimate slow generation —
/// chunks normally arrive ≪1s apart even on heavily-loaded local hardware.
const STREAM_INACTIVITY: Duration = Duration::from_secs(60);

/// First-chunk hint — emit a dimmed status line if no token has streamed yet
/// after this delay. Reassures the user the CLI is alive while a reasoning
/// model warms up its scratchpad.
const FIRST_CHUNK_HINT_AFTER: Duration = Duration::from_secs(5);

pub struct LMStudioBackend {
    client: Client,
    base_url: String,
    model: String,
}

impl LMStudioBackend {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        crate::ensure_crypto_provider();
        LMStudioBackend {
            // The shared pooled client applies a 10s connect timeout but no
            // total-request timeout, so slow local models can stream for as
            // long as they need.
            client: Client::new(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Fetch available model IDs from the LM Studio server.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        // `send()` errors on a non-2xx status, so a failed /v1/models surfaces
        // through `?` here.
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to connect to LM Studio")?;

        let body: Value = resp.json()?;
        let models = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }
}

impl ModelBackend for LMStudioBackend {
    fn name(&self) -> &str {
        "lmstudio"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn supports_tools(&self) -> bool {
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
            let oai_messages = serialize::openai_messages(messages, system);

            let mut body = json!({
                "model": self.model,
                "messages": oai_messages,
                "stream": true,
                "temperature": 0.7,
                "max_tokens": 8192,
            });

            if !tools.is_empty() {
                body["tools"] = serialize::openai_tools(tools);
                body["tool_choice"] = json!("auto");
            }

            let url = format!("{}/v1/chat/completions", self.base_url);
            let response = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .json(&body)
                .send_stream()
                .await
                .context("failed to connect to LM Studio")?;

            if !response.is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                // Detect "model can't load" — separate from generic errors so callers
                // can distinguish "model not available" from "bad request".
                if text.contains("Failed to load model")
                    || text.contains("insufficient system resources")
                {
                    bail!(
                        "model not available: {} — it may require more memory than is currently free. \
                       Try unloading other models first.",
                        self.model
                    );
                }
                bail!("LM Studio error {status}: {text}");
            }

            parse_lmstudio_stream(response, cancel, output, &self.model).await
        })
    }
}

async fn parse_lmstudio_stream(
    response: StreamResponse,
    cancel: CancellationToken,
    output: &OutputSink,
    model_id: &str,
) -> Result<BackendResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut result = BackendResponse::default();
    let mut got_first_chunk = false;
    let mut hinted_first_chunk_wait = false;

    // Reasoning models (deepseek-r1, qwen3-thinking, etc.) emit to
    // `reasoning_content`. We accumulate it separately so it can serve as the
    // response body when the model never produces non-empty `content`.
    let mut reasoning_accum = String::new();

    // Accumulate tool call deltas: index → (id, name, arguments)
    let mut tool_accum: HashMap<usize, (String, String, String)> = HashMap::new();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                sink_writeln(output, &format!("{}", "\n[interrupted]".yellow()));
                break;
            }
            // First-chunk hint — fires at most once, only while we're still waiting
            // on the very first byte. Keeps the user informed without polluting
            // mid-stream output.
            _ = tokio::time::sleep(FIRST_CHUNK_HINT_AFTER), if !got_first_chunk && !hinted_first_chunk_wait => {
                sink_writeln(
                    output,
                    &format!("{}", format!("  … waiting for first token from {model_id}").dimmed()),
                );
                hinted_first_chunk_wait = true;
                continue;
            }
            result = tokio::time::timeout(STREAM_INACTIVITY, stream.next()) => {
                match result {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(_) => bail!(
                        "lmstudio: no streamed chunk for {}s (model={model_id}) — \
                         server appears stuck. The model may be in an unbounded \
                         reasoning loop; try a more specific prompt or switch model.",
                        STREAM_INACTIVITY.as_secs(),
                    ),
                }
            }
        };
        got_first_chunk = true;
        let chunk = chunk.context("stream error")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

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

            let choice = match event["choices"].as_array().and_then(|a| a.first()) {
                Some(c) => c.clone(),
                None => continue,
            };

            // Token usage (some models send this in the final chunk)
            if let Some(usage) = event["usage"].as_object() {
                result.input_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(result.input_tokens as u64)
                    as u32;
                result.output_tokens = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(result.output_tokens as u64)
                    as u32;
            }

            let delta = &choice["delta"];
            let finish_reason = choice["finish_reason"].as_str();

            // Thinking/reasoning content (Qwen3, o1-style models) — stream dimmed
            // and accumulate so we can fall back to it if `content` is empty.
            if let Some(thinking) = delta["reasoning_content"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                sink_write(output, &format!("{}", thinking.dimmed()));
                reasoning_accum.push_str(thinking);
            }

            // Text content (the actual response)
            if let Some(text) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                sink_write(output, text);
                result.text.push_str(text);
            }

            // Tool calls (streamed as deltas per index)
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                for tc_delta in tool_calls {
                    let idx = tc_delta["index"].as_u64().unwrap_or(0) as usize;
                    let entry = tool_accum.entry(idx).or_insert_with(|| {
                        let id = tc_delta["id"].as_str().unwrap_or("").to_string();
                        let name = tc_delta["function"]["name"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        if !name.is_empty() {
                            sink_write(output, &format!("\n{}", format!("⚙ {name}").cyan()));
                        }
                        (id, name, String::new())
                    });

                    if let Some(args) = tc_delta["function"]["arguments"].as_str() {
                        entry.2.push_str(args);
                    }
                }
            }

            // Handle finish reason
            match finish_reason {
                Some("tool_calls") => {
                    result.stop_reason = StopReason::ToolUse;
                }
                Some("length") => {
                    result.stop_reason = StopReason::MaxTokens;
                }
                _ => {}
            }
        }
    }

    // Reasoning-only models (deepseek-r1 and similar) emit everything to
    // `reasoning_content` and leave `content` empty when the token budget is
    // consumed by thinking. Fall back to the reasoning text so the caller
    // gets a usable response instead of an "empty" error.
    if result.text.is_empty() && !reasoning_accum.is_empty() {
        result.text = reasoning_accum;
    }

    // If the stream ended without producing anything (and wasn't cancelled), surface a clear error
    // rather than returning an empty Ok that silently swallows the failure downstream.
    if result.text.is_empty() && tool_accum.is_empty() && !cancel.is_cancelled() {
        bail!("model returned an empty response — is the model loaded and responding correctly?");
    }

    // Flush accumulated tool calls
    let mut indexed: Vec<(usize, ToolCall)> = tool_accum
        .into_iter()
        .map(|(idx, (id, name, args_str))| {
            let input: Value = serde_json::from_str(&args_str).unwrap_or(json!({}));
            let id = if id.is_empty() { utils::id::new() } else { id };
            (idx, ToolCall { id, name, input })
        })
        .collect();
    indexed.sort_by_key(|(i, _)| *i);
    result.tool_calls = indexed.into_iter().map(|(_, tc)| tc).collect();

    if !result.text.is_empty() || !result.tool_calls.is_empty() {
        sink_writeln(output, "");
    }

    Ok(result)
}
