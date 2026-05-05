use super::{ModelBackend, OutputSink, serialize, sink_write, sink_writeln};
use types::{BackendResponse, Message, StopReason, ToolCall, ToolDef};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use colored::Colorize;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct LMStudioBackend {
    client: Client,
    base_url: String,
    model: String,
}

impl LMStudioBackend {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        LMStudioBackend {
            client: Client::builder()
                // Connect timeout only — no total-request timeout so slow local models
                // can take as long as they need to stream a response.
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Fetch available model IDs from the LM Studio server.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to connect to LM Studio")?;

        if !resp.status().is_success() {
            bail!("LM Studio /v1/models returned {}", resp.status());
        }

        let body: Value = resp.json().await?;
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

#[async_trait]
impl ModelBackend for LMStudioBackend {
    fn name(&self) -> &str {
        "lmstudio"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        system: &str,
        cancel: CancellationToken,
        output: &OutputSink,
    ) -> Result<BackendResponse> {
        let oai_messages = serialize::openai_messages(messages, system);

        let mut body = json!({
            "model": self.model,
            "messages": oai_messages,
            "stream": true,
            "temperature": 0.7,
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
            .send()
            .await
            .context("failed to connect to LM Studio")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("LM Studio error {status}: {text}");
        }

        parse_lmstudio_stream(response, cancel, output).await
    }
}

async fn parse_lmstudio_stream(
    response: reqwest::Response,
    cancel: CancellationToken,
    output: &OutputSink,
) -> Result<BackendResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut result = BackendResponse::default();

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
            chunk = stream.next() => {
                match chunk {
                    Some(c) => c,
                    None => break,
                }
            }
        };
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
            let id = if id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                id
            };
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

