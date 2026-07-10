#![allow(clippy::disallowed_types)] // local LLM HTTP passthrough — response shape not owned by orca
/// Lightweight local-LLM helper for non-streaming one-shot completions.
///
/// Supports LM Studio (OpenAI-compatible at `/v1/chat/completions`) and
/// Ollama (`/api/chat` with OpenAI-compat bridge or `/v1/chat/completions`).
///
/// These functions are used for search reranking and result presentation.
/// They never fall back to any cloud model — if all local providers are
/// unreachable or time out, the caller gets `None` and uses raw Rust results.
use serde_json::{Value, json};
use std::time::Duration;
use utils::http::Client;

/// A local LLM provider endpoint.
#[derive(Debug, Clone)]
pub struct LocalLlm {
    pub url: String,
    pub kind: LocalLlmKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalLlmKind {
    LmStudio,
    Ollama,
}

impl LocalLlm {
    pub fn lmstudio(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: LocalLlmKind::LmStudio,
        }
    }

    pub fn ollama(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: LocalLlmKind::Ollama,
        }
    }

    fn completions_url(&self) -> String {
        utils::url::join(&self.url, "v1/chat/completions")
    }
}

/// Try to discover available local LLM providers.
///
/// Probing order:
/// 1. DB-registered providers (from `orca engines add …`) — checked in parallel
/// 2. Env-var defaults: `LMSTUDIO_URL` (port 1234) and `OLLAMA_URL` (port 11434)
///
/// Returns the first reachable provider with at least one chat model.
pub async fn discover_local_llm() -> Option<LocalLlm> {
    // 1. DB-registered providers
    if let Ok(conn) = db::open_default()
        && let Ok(providers) = db::llm::list(&conn)
    {
        let enabled: Vec<_> = providers.into_iter().filter(|p| p.enabled).collect();
        if !enabled.is_empty() {
            let probes: Vec<_> = enabled
                .iter()
                .map(|p| {
                    let url = p.url.clone();
                    let kind = p.kind.clone();
                    async move {
                        let ok = if kind == "ollama" {
                            probe_ollama(&url).await
                        } else {
                            probe_lmstudio(&url).await
                        };
                        if ok {
                            Some(if kind == "ollama" {
                                LocalLlm::ollama(url)
                            } else {
                                LocalLlm::lmstudio(url)
                            })
                        } else {
                            None
                        }
                    }
                })
                .collect();
            let results = futures_util::future::join_all(probes).await;
            if let Some(llm) = results.into_iter().flatten().next() {
                return Some(llm);
            }
        }
    }

    // 2. Env-var defaults
    let lms_url =
        std::env::var("LMSTUDIO_URL").unwrap_or_else(|_| "http://localhost:1234".to_string());
    let ollama_url =
        std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".to_string());

    let (lms, ollama) = tokio::join!(probe_lmstudio(&lms_url), probe_ollama(&ollama_url),);

    // Prefer LM Studio; fall back to Ollama
    if lms {
        return Some(LocalLlm::lmstudio(lms_url));
    }
    if ollama {
        return Some(LocalLlm::ollama(ollama_url));
    }
    None
}

async fn probe_lmstudio(base_url: &str) -> bool {
    // Fast probe: give up on a dead endpoint in 500ms (connect) / 2s (total).
    let client = Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .build();

    // `send()` errors on a non-2xx status, so a bad status hits this else.
    let Ok(resp) = client
        .get(utils::url::join(base_url, "v1/models"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
    else {
        return false;
    };

    let Ok(val) = resp.json::<Value>() else {
        return false;
    };
    val["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|m| !m["id"].as_str().unwrap_or("").contains("embed"))
        })
        .unwrap_or(false)
}

async fn probe_ollama(base_url: &str) -> bool {
    // Fast probe: give up on a dead endpoint in 500ms (connect) / 2s (total).
    let client = Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .build();

    // Ollama exposes /api/tags or /v1/models (via OpenAI compat layer).
    // `send()` succeeds only on a 2xx, so reachability == `is_ok`.
    client
        .get(utils::url::join(base_url, "api/tags"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

/// POST a single-turn prompt to a local LLM and return the response text.
/// Returns `None` on any error, connection failure, or timeout.
pub async fn complete(llm: &LocalLlm, prompt: &str, timeout_ms: u64) -> Option<String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build();

    let body = json!({
        "model": "",
        "messages": [{ "role": "user", "content": prompt }],
        "stream": false,
        "max_tokens": 2000,
        "temperature": 0.1
    });

    // `send()` returns `Err` on a non-2xx status, so `.ok()?` folds the old
    // explicit status check into the `None` path.
    let resp = client
        .post(llm.completions_url())
        .header("content-type", "application/json")
        .json(&body)
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
        .ok()?;

    let val: Value = resp.json().ok()?;
    val["choices"][0]["message"]["content"]
        .as_str()
        .map(String::from)
}

/// Rerank and filter a JSON search result list using a local LLM.
///
/// Input: `[{root, path, matches}]`. Returns the same structure sorted by
/// relevance, with irrelevant entries removed. Returns `None` if no local
/// LLM is available or the response can't be parsed.
pub async fn rerank_results(
    llm: &LocalLlm,
    query: &str,
    results: &[Value],
    timeout_ms: u64,
) -> Option<Vec<Value>> {
    if results.is_empty() {
        return None;
    }
    let results_json = serde_json::to_string(results).ok()?;
    let prompt = format!(
        "Search query: \"{query}\"\n\n\
         Results (JSON):\n{results_json}\n\n\
         Return a JSON array of the most relevant results, sorted by relevance, \
         most relevant first. Remove results irrelevant to the query. \
         Keep the same structure: [{{\"root\":\"...\",\"path\":\"...\",\"matches\":[...]}}]. \
         Return only valid JSON, no explanation."
    );

    let text = complete(llm, &prompt, timeout_ms).await?;
    extract_json_array(&text)
}

/// Format raw text search results into a readable summary using a local LLM.
///
/// Input is the raw text from `search_docs` (path + matching lines).
/// Returns a formatted, relevance-sorted presentation, or `None` if no
/// local LLM is available.
pub async fn present_text_results(
    llm: &LocalLlm,
    query: &str,
    raw_results: &str,
    timeout_ms: u64,
) -> Option<String> {
    if raw_results.trim().is_empty() {
        return None;
    }
    let prompt = format!(
        "Search query: \"{query}\"\n\nRaw results:\n{raw_results}\n\n\
         Present the most relevant results in a concise, readable format. \
         Group related results, note what each file covers, and explain \
         relevance briefly. Omit clearly irrelevant results. \
         Prioritize signal over completeness."
    );

    complete(llm, &prompt, timeout_ms).await
}

/// Extract the first JSON array from an LLM response, handling markdown fences.
fn extract_json_array(text: &str) -> Option<Vec<Value>> {
    let text = text.trim();
    let inner = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|s| s.trim_start_matches('\n'))
        .unwrap_or(text)
        .trim_end_matches("```")
        .trim();

    let start = inner.find('[')?;
    let end = inner.rfind(']').map(|i| i + 1)?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&inner[start..end]).ok()
}
