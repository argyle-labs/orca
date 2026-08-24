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
        let url = utils::url::join(&self.base_url, "api/tags");
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
        let url = utils::url::join(&self.base_url, "v1/models");
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

            let url = utils::url::join(&self.base_url, "v1/chat/completions");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ModelBackend, buffer_sink};

    // ── OllamaBackend construction / accessors ────────────────────────────────

    #[test]
    fn new_stores_base_url_and_model() {
        let b = OllamaBackend::new("http://localhost:11434", "qwen3:8b");
        assert_eq!(b.base_url, "http://localhost:11434");
        assert_eq!(b.model, "qwen3:8b");
    }

    #[test]
    fn new_accepts_owned_strings() {
        let b = OllamaBackend::new(String::from("http://host:1/"), String::from("llama3"));
        assert_eq!(b.base_url, "http://host:1/");
        assert_eq!(b.model, "llama3");
    }

    #[test]
    fn name_is_ollama() {
        let b = OllamaBackend::new("http://localhost:11434", "m");
        assert_eq!(b.name(), "ollama");
    }

    #[test]
    fn model_id_returns_configured_model() {
        let b = OllamaBackend::new("http://localhost:11434", "deepseek-r1:14b");
        assert_eq!(b.model_id(), "deepseek-r1:14b");
    }

    #[test]
    fn supports_tools_is_true() {
        let b = OllamaBackend::new("http://localhost:11434", "m");
        assert!(b.supports_tools());
    }

    #[test]
    fn ollama_is_local_by_default() {
        let b = OllamaBackend::new("http://localhost:11434", "m");
        assert!(b.is_local());
    }

    // ── request body / URL construction ───────────────────────────────────────
    //
    // Reconstruct the exact chat payload the way `chat()` builds it, then assert
    // on the serialized JSON string (no Value inspection). This exercises the
    // same serialize helpers and json shape used on the wire.

    fn user_msg(s: &str) -> Message {
        Message::User {
            content: s.to_string(),
        }
    }

    #[test]
    fn chat_body_without_tools_omits_tool_fields() {
        let messages = [user_msg("hello")];
        let oai_messages = serialize::openai_messages(&messages, "sys");
        let body = json!({
            "model": "qwen3:8b",
            "messages": oai_messages,
            "stream": true,
            "max_tokens": 8192,
        });
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("\"model\":\"qwen3:8b\""));
        assert!(s.contains("\"stream\":true"));
        assert!(s.contains("\"max_tokens\":8192"));
        assert!(s.contains("\"role\":\"system\""));
        assert!(s.contains("\"content\":\"sys\""));
        assert!(!s.contains("\"tools\""));
        assert!(!s.contains("\"tool_choice\""));
    }

    #[test]
    fn chat_body_with_tools_adds_tools_and_auto_choice() {
        let messages = [user_msg("hello")];
        let tools = [ToolDef {
            name: "bash".to_string(),
            description: "run a command".to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "required": [] }),
        }];
        let oai_messages = serialize::openai_messages(&messages, "");
        let mut body = json!({
            "model": "m",
            "messages": oai_messages,
            "stream": true,
            "max_tokens": 8192,
        });
        if !tools.is_empty() {
            body["tools"] = serialize::openai_tools(&tools);
            body["tool_choice"] = json!("auto");
        }
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("\"tool_choice\":\"auto\""));
        assert!(s.contains("\"type\":\"function\""));
        assert!(s.contains("\"name\":\"bash\""));
    }

    #[test]
    fn chat_url_is_openai_compat_completions() {
        let url = utils::url::join("http://localhost:11434", "v1/chat/completions");
        assert!(url.ends_with("/v1/chat/completions"));
    }

    #[test]
    fn list_models_urls_are_well_formed() {
        assert!(utils::url::join("http://h:1", "api/tags").ends_with("/api/tags"));
        assert!(utils::url::join("http://h:1", "v1/models").ends_with("/v1/models"));
        // Trailing slash in base should not double up.
        assert_eq!(
            utils::url::join("http://h:1/", "api/tags"),
            utils::url::join("http://h:1", "api/tags")
        );
    }

    // ── filter_think_tokens ───────────────────────────────────────────────────

    fn run_filter(chunk: &str, in_think: &mut bool, buf: &mut String) -> String {
        let (sink, _) = buffer_sink();
        filter_think_tokens(chunk, in_think, buf, &sink)
    }

    #[test]
    fn filter_passes_plain_text_unchanged() {
        let mut in_think = false;
        let mut buf = String::new();
        let visible = run_filter("just some text", &mut in_think, &mut buf);
        assert_eq!(visible, "just some text");
        assert!(!in_think);
        assert!(buf.is_empty());
    }

    #[test]
    fn filter_strips_complete_think_block() {
        let mut in_think = false;
        let mut buf = String::new();
        let visible = run_filter("before<think>hidden</think>after", &mut in_think, &mut buf);
        assert_eq!(visible, "beforeafter");
        assert!(!in_think);
        assert!(buf.is_empty());
    }

    #[test]
    fn filter_enters_think_on_unterminated_open() {
        let mut in_think = false;
        let mut buf = String::new();
        let visible = run_filter("visible<think>partial thought", &mut in_think, &mut buf);
        assert_eq!(visible, "visible");
        assert!(in_think);
        assert_eq!(buf, "partial thought");
    }

    #[test]
    fn filter_resumes_across_chunk_boundary() {
        let mut in_think = false;
        let mut buf = String::new();
        // First chunk opens the think block and leaves us inside it.
        let v1 = run_filter("a<think>thinking ", &mut in_think, &mut buf);
        assert_eq!(v1, "a");
        assert!(in_think);
        // Second chunk closes it and resumes visible output.
        let v2 = run_filter("more</think>done", &mut in_think, &mut buf);
        assert_eq!(v2, "done");
        assert!(!in_think);
        assert!(buf.is_empty());
    }

    #[test]
    fn filter_starts_inside_think_block() {
        let mut in_think = true;
        let mut buf = String::new();
        let visible = run_filter("still hidden", &mut in_think, &mut buf);
        assert_eq!(visible, "");
        assert!(in_think);
        assert_eq!(buf, "still hidden");
    }

    #[test]
    fn filter_handles_multiple_think_blocks_in_one_chunk() {
        let mut in_think = false;
        let mut buf = String::new();
        let visible = run_filter(
            "x<think>a</think>y<think>b</think>z",
            &mut in_think,
            &mut buf,
        );
        assert_eq!(visible, "xyz");
        assert!(!in_think);
    }

    // ── dedupe_paragraphs ─────────────────────────────────────────────────────

    #[test]
    fn dedupe_leaves_unique_paragraphs() {
        let text = "first paragraph\n\nsecond paragraph";
        assert_eq!(dedupe_paragraphs(text), text);
    }

    #[test]
    fn dedupe_removes_exact_duplicate() {
        let text = "hello world this is a repeated line\n\nhello world this is a repeated line";
        let out = dedupe_paragraphs(text);
        assert_eq!(out, "hello world this is a repeated line");
    }

    #[test]
    fn dedupe_removes_prefix_duplicate() {
        let long = "the quick brown fox jumps over the lazy dog again and again forever";
        let text = format!("{long}\n\n{long} plus extra tail content here");
        let out = dedupe_paragraphs(&text);
        // Second paragraph shares the first 60 chars, so it is dropped.
        assert_eq!(out, long);
    }

    #[test]
    fn dedupe_preserves_blank_paragraphs() {
        let text = "alpha\n\n\n\nbeta";
        let out = dedupe_paragraphs(text);
        assert_eq!(out, text);
    }

    #[test]
    fn dedupe_empty_string_is_empty() {
        assert_eq!(dedupe_paragraphs(""), "");
    }

    // ── end-to-end over a local mock HTTP server ──────────────────────────────
    //
    // `list_models`, `chat`, and `parse_ollama_stream` all take a real
    // `StreamResponse` / go through the reqwest-backed `Client`, so we exercise
    // them against a throwaway TCP server that speaks just enough HTTP/1.1 to
    // return canned bodies. The handler routes by request path so a single
    // server can back both the native `/api/tags` and fallback `/v1/models`
    // probes in `list_models`.

    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn http_response(status: u16, body: &str) -> String {
        let reason = if status == 200 { "OK" } else { "ERR" };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Spawn a mock server. `handler` receives the request line + path and
    /// returns `(status, body)`. Returns the base URL to hand to the backend.
    async fn spawn_server<F>(handler: F) -> String
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.lines().next().unwrap_or("").to_string();
                    let (status, body) = handler(&path);
                    sock.write_all(http_response(status, &body).as_bytes())
                        .await
                        .ok();
                    sock.flush().await.ok();
                });
            }
        });
        format!("http://{addr}")
    }

    /// Build an SSE `data:` stream body from raw JSON event payloads.
    fn sse(events: &[&str]) -> String {
        let mut s = String::new();
        for e in events {
            s.push_str("data: ");
            s.push_str(e);
            s.push('\n');
        }
        s.push_str("data: [DONE]\n");
        s
    }

    #[tokio::test]
    async fn list_models_uses_native_tags() {
        let url = spawn_server(|_path| {
            (
                200,
                r#"{"models":[{"name":"qwen3:8b"},{"name":"llama3"}]}"#.to_string(),
            )
        })
        .await;
        let b = OllamaBackend::new(url, "m");
        let models = b.list_models().await.unwrap();
        assert_eq!(models, vec!["qwen3:8b".to_string(), "llama3".to_string()]);
    }

    #[tokio::test]
    async fn list_models_falls_back_to_v1_models() {
        // /api/tags 500s (send() errors) so we drop through to /v1/models.
        let url = spawn_server(|path| {
            if path.contains("api/tags") {
                (500, "nope".to_string())
            } else {
                (
                    200,
                    r#"{"data":[{"id":"gpt-oss"},{"id":"phi"}]}"#.to_string(),
                )
            }
        })
        .await;
        let b = OllamaBackend::new(url, "m");
        let models = b.list_models().await.unwrap();
        assert_eq!(models, vec!["gpt-oss".to_string(), "phi".to_string()]);
    }

    #[tokio::test]
    async fn list_models_empty_when_no_arrays() {
        let url = spawn_server(|path| {
            if path.contains("api/tags") {
                // "models" present but not an array -> as_array() is None, fall through.
                (200, r#"{"models":null}"#.to_string())
            } else {
                // "data" present but not an array -> unwrap_or_default gives empty vec.
                (200, r#"{"data":"nope"}"#.to_string())
            }
        })
        .await;
        let b = OllamaBackend::new(url, "m");
        let models = b.list_models().await.unwrap();
        assert!(models.is_empty());
    }

    fn run_chat(url: String) -> Result<BackendResponse> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let b = OllamaBackend::new(url, "m");
            let (sink, _) = buffer_sink();
            let msgs = [user_msg("hi")];
            b.chat(&msgs, &[], "sys", CancellationToken::new(), &sink)
                .await
        })
    }

    #[test]
    fn chat_collects_streamed_text_and_usage() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(spawn_server(|_p| {
            let body = sse(&[
                r#"{"choices":[{"delta":{"content":"Hello "},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"content":"world"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
            ]);
            (200, body)
        }));
        let resp = run_chat(url).unwrap();
        assert_eq!(resp.text, "Hello world");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.input_tokens, 11);
        assert_eq!(resp.output_tokens, 7);
    }

    #[test]
    fn chat_strips_leaked_think_blocks_from_content() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(spawn_server(|_p| {
            let body = sse(&[
                r#"{"choices":[{"delta":{"content":"a<think>secret</think>b"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]);
            (200, body)
        }));
        let resp = run_chat(url).unwrap();
        assert_eq!(resp.text, "ab");
    }

    #[test]
    fn chat_accumulates_tool_calls() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(spawn_server(|_p| {
            let body = sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"cmd\":"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]);
            (200, body)
        }));
        let resp = run_chat(url).unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.tool_calls.len(), 1);
        let tc = &resp.tool_calls[0];
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "bash");
        assert_eq!(tc.input["cmd"], json!("ls"));
    }

    #[test]
    fn chat_reasoning_tokens_do_not_appear_in_text() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(spawn_server(|_p| {
            let body = sse(&[
                r#"{"choices":[{"delta":{"reasoning":"pondering"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{"thinking":"more"},"finish_reason":null}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]);
            (200, body)
        }));
        // Empty visible text but a proper stop signal => clean EndTurn, no bail.
        let resp = run_chat(url).unwrap();
        assert!(resp.text.is_empty());
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn chat_length_finish_maps_to_max_tokens() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(spawn_server(|_p| {
            let body =
                sse(&[r#"{"choices":[{"delta":{"content":"cut"},"finish_reason":"length"}]}"#]);
            (200, body)
        }));
        let resp = run_chat(url).unwrap();
        assert_eq!(resp.text, "cut");
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn chat_errors_on_non_success_status() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = rt.block_on(spawn_server(|_p| (500, "model exploded".to_string())));
        let err = run_chat(url).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Ollama error 500"), "got: {msg}");
        assert!(msg.contains("model exploded"), "got: {msg}");
    }

    #[test]
    fn chat_empty_stream_without_stop_bails() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        // Only [DONE]; no content, no tool calls, no stop signal.
        let url = rt.block_on(spawn_server(|_p| (200, "data: [DONE]\n".to_string())));
        let err = run_chat(url).unwrap_err();
        assert!(format!("{err}").contains("empty response"));
    }

    #[test]
    fn chat_dedupes_repeated_paragraphs_in_stream() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dup = "the exact same long paragraph repeated twice over and over here";
        let events = format!(
            r#"{{"choices":[{{"delta":{{"content":"{dup}\n\n{dup}"}},"finish_reason":"stop"}}]}}"#
        );
        let url = rt.block_on(spawn_server(move |_p| (200, sse(&[events.as_str()]))));
        let resp = run_chat(url).unwrap();
        assert_eq!(resp.text, dup);
    }
}
