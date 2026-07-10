// Ollama provider SDK request/response envelopes; HashMap/Value are wire-format passthrough.
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

pub struct OllamaBackend {
    client: Client,
    base_url: String,
    model: String,
}

impl OllamaBackend {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        crate::ensure_crypto_provider();
        OllamaBackend {
            client: Client::new(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        // Prefer native /api/tags endpoint; fall back to OpenAI-compat /v1/models.
        // `send()` errors on a non-2xx status, so a failed /api/tags simply
        // drops through to the fallback below.
        let url = format!("{}/api/tags", self.base_url);
        if let Ok(resp) = self.client.get(&url).send().await {
            let body: Value = resp.json()?;
            if let Some(arr) = body["models"].as_array() {
                return Ok(arr
                    .iter()
                    .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                    .collect());
            }
        }

        // OpenAI-compat fallback
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to connect to Ollama /v1/models")?;
        let body: Value = resp.json()?;
        Ok(body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl ModelBackend for OllamaBackend {
    fn name(&self) -> &str {
        "ollama"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    // Ollama handles tool routing server-side for capable models.
    fn supports_tools(&self) -> bool {
        true
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
                .context("failed to connect to Ollama")?;

            if !response.is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("Ollama error {status}: {text}");
            }

            parse_ollama_stream(response, cancel, output).await
        })
    }
}

async fn parse_ollama_stream(
    response: StreamResponse,
    cancel: CancellationToken,
    output: &OutputSink,
) -> Result<BackendResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut result = BackendResponse::default();
    let mut tool_accum: HashMap<usize, (String, String, String)> = HashMap::new();
    // Qwen3 and similar models emit thinking inside <think>...</think> in the content stream.
    let mut in_think_block = false;
    let mut think_buf = String::new();
    // Track whether the server sent a proper stop signal — empty content with "stop" is EndTurn.
    let mut saw_stop = false;

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

            // Ollama-native thinking/reasoning field (Qwen3, DeepSeek R1 via Ollama).
            // Ollama uses "reasoning" in its OpenAI-compat stream; some builds use "thinking".
            let reasoning_token = delta["reasoning"]
                .as_str()
                .or_else(|| delta["thinking"].as_str())
                .filter(|s| !s.is_empty());
            if let Some(thinking) = reasoning_token {
                sink_write(output, &format!("{}", thinking.dimmed()));
            }

            if let Some(text) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                // Filter <think>...</think> blocks that leak into content.
                let visible =
                    filter_think_tokens(text, &mut in_think_block, &mut think_buf, output);
                if !visible.is_empty() {
                    sink_write(output, &visible);
                    result.text.push_str(&visible);
                }
            }

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

            match finish_reason {
                Some("tool_calls") => {
                    result.stop_reason = StopReason::ToolUse;
                }
                Some("length") => {
                    result.stop_reason = StopReason::MaxTokens;
                }
                Some("stop") => {
                    saw_stop = true;
                }
                _ => {}
            }
        }
    }

    // Empty content with a proper stop signal is EndTurn — model finished cleanly.
    // Only bail when nothing arrived at all and no stop was signalled (model not loaded / crashed).
    if result.text.is_empty() && tool_accum.is_empty() && !cancel.is_cancelled() && !saw_stop {
        bail!("Ollama returned an empty response — is the model loaded?");
    }

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

    // Deduplicate repeated paragraphs — local models sometimes echo themselves.
    if !result.text.is_empty() {
        result.text = dedupe_paragraphs(&result.text);
    }

    Ok(result)
}

/// Pass content tokens through, stripping `<think>…</think>` blocks.
/// Returns the visible (non-thinking) portion of `chunk`.
fn filter_think_tokens(
    chunk: &str,
    in_think: &mut bool,
    buf: &mut String,
    output: &OutputSink,
) -> String {
    let mut visible = String::new();
    let mut rest = chunk;
    loop {
        if *in_think {
            if let Some(end) = rest.find("</think>") {
                // Flush accumulated thinking dimmed.
                buf.push_str(&rest[..end]);
                sink_write(output, &format!("{}", buf.dimmed()));
                buf.clear();
                *in_think = false;
                rest = &rest[end + "</think>".len()..];
            } else {
                buf.push_str(rest);
                break;
            }
        } else if let Some(start) = rest.find("<think>") {
            visible.push_str(&rest[..start]);
            *in_think = true;
            rest = &rest[start + "<think>".len()..];
        } else {
            visible.push_str(rest);
            break;
        }
    }
    visible
}

/// Remove duplicate paragraphs from a response — local models sometimes repeat themselves.
fn dedupe_paragraphs(text: &str) -> String {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<&str> = Vec::new();
    for p in &paragraphs {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            out.push(p);
            continue;
        }
        // Check if this paragraph is substantially the same as one already seen
        // (exact match or starts with the same 60 chars).
        let key = &trimmed[..trimmed.len().min(60)];
        if seen.iter().any(|s| s.trim().starts_with(key)) {
            continue; // skip duplicate
        }
        seen.push(trimmed);
        out.push(p);
    }
    out.join("\n\n")
}
