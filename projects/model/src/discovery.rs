//! Dynamic model discovery and task-aware selection.
//!
//! At any point in time, the set of available models is whatever is actually
//! reachable right now — LM Studio models currently loaded, Claude if a key
//! is configured, Ollama if it's running. This module queries all configured
//! backends, classifies each model's capabilities from its ID heuristics, and
//! selects the best model for a given task kind.
//!
//! No model is hardcoded as "the" model. Selection is always driven by what's
//! literally available at call time.

use crate::backend::{ClaudeBackend, LMStudioBackend, OllamaBackend};
use contract::config::Config;
use futures_util::future::join_all;

// ── Task classification ───────────────────────────────────────────────────────

/// The kind of work being requested. Drives model selection when multiple
/// models are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    /// Write, edit, explain, or review code.
    Coding,
    /// Multi-step reasoning, math, logic, planning.
    Reasoning,
    /// Call external tools / functions. Requires reliable JSON output.
    ToolUse,
    /// Summarize, compare, or analyze text.
    Analysis,
    /// General conversation / question answering.
    Chat,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Coding => "coding",
            TaskKind::Reasoning => "reasoning",
            TaskKind::ToolUse => "tool_use",
            TaskKind::Analysis => "analysis",
            TaskKind::Chat => "chat",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "coding" => Some(TaskKind::Coding),
            "reasoning" => Some(TaskKind::Reasoning),
            "tool_use" => Some(TaskKind::ToolUse),
            "analysis" => Some(TaskKind::Analysis),
            "chat" => Some(TaskKind::Chat),
            _ => None,
        }
    }
}

// ── Model capabilities ────────────────────────────────────────────────────────

/// Capabilities inferred from a model's ID and backend.
///
/// These are heuristics — real metadata would come from model cards or
/// provider APIs. The heuristics are conservative: when in doubt, mark as
/// not supported rather than assuming.
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    /// Whether the model reliably produces valid JSON for tool arguments.
    pub supports_tools: bool,
    /// Whether this is a chain-of-thought / thinking model (slower, better at math).
    pub is_reasoning: bool,
    /// Estimated context window in tokens.
    pub context_window: usize,
    /// Tasks this model is best suited for, in priority order.
    pub preferred_tasks: Vec<TaskKind>,
    /// Selection rank when multiple models match equally — lower wins.
    pub rank: u8,
}

/// Infer a model's capabilities from its ID string and backend name.
///
/// The heuristics here should be updated as new model families become
/// common. The goal is to avoid selecting a reasoning-only model for
/// tool-calling tasks, and to prefer code-specialized models for coding.
pub fn classify_model(id: &str, backend: &str) -> ModelCapabilities {
    let id_lower = id.to_ascii_lowercase();

    let is_claude = backend == "claude" || id_lower.starts_with("claude");
    let is_reasoning = id_lower.contains("deepseek-r1")
        || id_lower.contains("/r1-")
        || id_lower.contains("-r1-")
        || id_lower.contains("o1-")
        || id_lower.contains("o3-")
        || id_lower.contains("-thinking")
        || id_lower.contains("reasoning");
    let is_qwen = id_lower.starts_with("qwen") || id_lower.contains("/qwen");
    let is_code = id_lower.contains("code")
        || id_lower.contains("coder")
        || id_lower.contains("codestral")
        || id_lower.contains("deepseek-coder")
        || id_lower.contains("starcoder");
    let is_small = id_lower.contains("0.5b")
        || id_lower.contains("1b-")
        || id_lower.contains("1.5b")
        || id_lower.contains("3b-");
    let is_embed = id_lower.contains("embed") || id_lower.contains("embedding");

    // Reasoning models generally can't produce structured tool JSON reliably.
    // Claude is the exception — it's a reasoning model that also does tools.
    let supports_tools = if is_embed {
        false
    } else if is_reasoning {
        is_claude
    } else {
        true
    };

    let context_window = if is_claude {
        200_000
    } else if id_lower.contains("128k") {
        131_072
    } else if id_lower.contains("32k") {
        32_768
    } else if id_lower.contains("16k") {
        16_384
    } else {
        32_768
    };

    let preferred_tasks = if is_embed {
        vec![]
    } else if is_claude {
        vec![
            TaskKind::Coding,
            TaskKind::ToolUse,
            TaskKind::Analysis,
            TaskKind::Reasoning,
            TaskKind::Chat,
        ]
    } else if is_reasoning {
        vec![TaskKind::Reasoning, TaskKind::Analysis]
    } else if is_code {
        vec![TaskKind::Coding, TaskKind::ToolUse]
    } else if is_qwen {
        vec![
            TaskKind::ToolUse,
            TaskKind::Coding,
            TaskKind::Chat,
            TaskKind::Analysis,
        ]
    } else {
        vec![TaskKind::Chat, TaskKind::Analysis]
    };

    // Rank: lower = preferred when capabilities are otherwise equal.
    // Check small first — a small qwen is still a small model.
    let rank: u8 = if is_embed {
        255
    } else if is_small {
        50
    }
    // small models — deprioritize regardless of family
    else if is_claude {
        5
    }
    // cloud API — great but costs money
    else if is_qwen && !is_reasoning {
        10
    }
    // best local all-rounder
    else if is_code {
        15
    }
    // specialized code models
    else if is_reasoning {
        60
    }
    // reasoning-only: slow, narrow
    else {
        30
    }; // other chat models

    ModelCapabilities {
        supports_tools,
        is_reasoning,
        context_window,
        preferred_tasks,
        rank,
    }
}

// ── Discovered model ──────────────────────────────────────────────────────────

/// A model that is actually available right now on some backend.
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    /// The model identifier as returned by the backend (e.g. "qwen/qwen3-8b").
    pub id: String,
    /// Which backend serves this model: "lmstudio", "claude", "ollama".
    pub backend: String,
    /// The base URL of the backend that serves this model. Empty for Claude.
    pub url: String,
    /// Inferred capabilities.
    pub capabilities: ModelCapabilities,
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Query all configured backends and return every model that is actually
/// available right now. Backends that are unreachable are silently skipped —
/// availability is dynamic, not an error condition.
///
/// Embedding models are excluded (they can't handle chat completions).
pub async fn discover_all(config: &Config) -> Vec<DiscoveredModel> {
    // Collect all (kind, url) pairs to probe: env-var defaults + DB registrations.
    let mut endpoints: Vec<(String, String)> = vec![
        ("lmstudio".into(), config.lmstudio_url.clone()),
        ("ollama".into(), config.ollama_url.clone()),
    ];

    // DB-registered providers override or supplement the env defaults.
    if let Ok(conn) = db::open(&config.db_path)
        && let Ok(providers) = db::llm::list(&conn)
    {
        for p in providers.into_iter().filter(|p| p.enabled) {
            // If there's already an entry for this URL, skip (dedup).
            if !endpoints.iter().any(|(_, u)| u == &p.url) {
                endpoints.push((p.kind.clone(), p.url.clone()));
            }
        }
    }

    // Probe all endpoints concurrently.
    let probes: Vec<_> = endpoints
        .into_iter()
        .map(|(kind, url)| async move {
            let ids = match kind.as_str() {
                "ollama" => OllamaBackend::new(&url, "").list_models().await.ok(),
                _ => LMStudioBackend::new(&url, "").list_models().await.ok(),
            };
            ids.unwrap_or_default()
                .into_iter()
                .filter_map(|id| {
                    let caps = classify_model(&id, &kind);
                    if caps.preferred_tasks.is_empty() {
                        return None;
                    }
                    Some(DiscoveredModel {
                        id,
                        backend: kind.clone(),
                        url: url.clone(),
                        capabilities: caps,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let mut found: Vec<DiscoveredModel> = join_all(probes).await.into_iter().flatten().collect();

    // Claude — available if a key is configured.
    if config.anthropic_api_key.is_some() {
        for claude_id in ClaudeBackend::known_models() {
            found.push(DiscoveredModel {
                id: claude_id.to_string(),
                backend: "claude".into(),
                url: String::new(),
                capabilities: classify_model(claude_id, "claude"),
            });
        }
    }

    found
}

// ── Selection ─────────────────────────────────────────────────────────────────

/// Select the best available model for a given task.
///
/// Scoring (lower = better):
///   1. Tool-capable models rank above non-capable when task is ToolUse.
///   2. Models with the task in their preferred list rank above those without.
///   3. Within the same tier, rank (lower = preferred hardware/family) breaks ties.
pub fn select_for_task(models: &[DiscoveredModel], task: TaskKind) -> Option<&DiscoveredModel> {
    if models.is_empty() {
        return None;
    }
    models.iter().min_by_key(|m| {
        // Hard penalty: don't use a tool-incapable model for ToolUse.
        let tools_penalty: u16 = if task == TaskKind::ToolUse && !m.capabilities.supports_tools {
            1000
        } else {
            0
        };
        // Primary: does this model prefer this task?
        let task_match: u16 = if m.capabilities.preferred_tasks.contains(&task) {
            0
        } else {
            100
        };
        // Tiebreaker: family/hardware rank.
        let rank: u16 = m.capabilities.rank as u16;

        tools_penalty + task_match + rank
    })
}

// ── Build a Model from a DiscoveredModel ─────────────────────────────────────

/// Convert a DiscoveredModel back into the contract::config::Model enum so it can be
/// passed to `build_backend`.
pub fn to_config_model(discovered: &DiscoveredModel) -> contract::config::Model {
    match discovered.backend.as_str() {
        "claude" => contract::config::Model::Claude(discovered.id.clone()),
        "ollama" => contract::config::Model::Ollama {
            id: discovered.id.clone(),
            url: discovered.url.clone(),
        },
        _ => contract::config::Model::LMStudio {
            id: discovered.id.clone(),
            url: discovered.url.clone(),
        },
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_model ────────────────────────────────────────────────────────

    #[test]
    fn claude_model_full_capabilities() {
        let caps = classify_model("claude-sonnet-4-6", "claude");
        assert!(caps.supports_tools);
        assert!(!caps.is_reasoning);
        assert_eq!(caps.context_window, 200_000);
        assert!(caps.preferred_tasks.contains(&TaskKind::ToolUse));
        assert!(caps.preferred_tasks.contains(&TaskKind::Coding));
        assert_eq!(caps.rank, 5);
    }

    #[test]
    fn qwen_chat_preferred_for_tool_use() {
        let caps = classify_model("qwen/qwen3-8b", "lmstudio");
        assert!(caps.supports_tools);
        assert!(!caps.is_reasoning);
        assert_eq!(caps.preferred_tasks[0], TaskKind::ToolUse);
        assert_eq!(caps.rank, 10);
    }

    #[test]
    fn deepseek_r1_is_reasoning_no_tools() {
        let caps = classify_model("deepseek-r1-distill-qwen-14b", "lmstudio");
        assert!(caps.is_reasoning);
        assert!(!caps.supports_tools);
        assert!(caps.preferred_tasks.contains(&TaskKind::Reasoning));
        assert!(!caps.preferred_tasks.contains(&TaskKind::ToolUse));
        assert!(caps.rank >= 50);
    }

    #[test]
    fn embed_model_excluded() {
        let caps = classify_model("nomic-embed-text", "lmstudio");
        assert!(caps.preferred_tasks.is_empty());
        assert!(!caps.supports_tools);
        assert_eq!(caps.rank, 255);
    }

    #[test]
    fn code_model_preferred_for_coding() {
        let caps = classify_model("codestral-22b", "lmstudio");
        assert!(caps.preferred_tasks.contains(&TaskKind::Coding));
        assert!(caps.preferred_tasks.contains(&TaskKind::ToolUse));
        assert!(caps.rank < 30);
    }

    #[test]
    fn small_model_deprioritized() {
        let caps = classify_model("qwen2.5-0.5b-instruct", "lmstudio");
        assert!(
            caps.rank >= 50,
            "small model should rank low, got {}",
            caps.rank
        );
    }

    #[test]
    fn o1_style_model_is_reasoning() {
        let caps = classify_model("o1-mini", "lmstudio");
        assert!(caps.is_reasoning);
    }

    #[test]
    fn thinking_model_is_reasoning() {
        let caps = classify_model("qwen3-thinking-14b", "lmstudio");
        assert!(caps.is_reasoning);
        assert!(!caps.supports_tools);
    }

    #[test]
    fn context_window_128k_parsed() {
        let caps = classify_model("llama-3.1-8b-128k", "lmstudio");
        assert_eq!(caps.context_window, 131_072);
    }

    // ── select_for_task ───────────────────────────────────────────────────────

    fn make_model(id: &str, backend: &str) -> DiscoveredModel {
        DiscoveredModel {
            id: id.to_string(),
            backend: backend.to_string(),
            url: String::new(),
            capabilities: classify_model(id, backend),
        }
    }

    #[test]
    fn empty_list_returns_none() {
        assert!(select_for_task(&[], TaskKind::Chat).is_none());
    }

    #[test]
    fn single_model_always_selected() {
        let models = vec![make_model("some-model", "lmstudio")];
        assert!(select_for_task(&models, TaskKind::Chat).is_some());
    }

    #[test]
    fn tool_use_prefers_tool_capable_model() {
        let models = vec![
            make_model("deepseek-r1-14b", "lmstudio"), // reasoning, no tools
            make_model("qwen/qwen3-8b", "lmstudio"),   // chat qwen, tools ok
        ];
        let selected = select_for_task(&models, TaskKind::ToolUse).unwrap();
        assert_eq!(
            selected.id, "qwen/qwen3-8b",
            "reasoning model should not be selected for tool use"
        );
    }

    #[test]
    fn reasoning_task_prefers_reasoning_model() {
        let models = vec![
            make_model("qwen/qwen3-8b", "lmstudio"),
            make_model("deepseek-r1-14b", "lmstudio"),
        ];
        let selected = select_for_task(&models, TaskKind::Reasoning).unwrap();
        assert_eq!(
            selected.id, "deepseek-r1-14b",
            "should prefer reasoning model for reasoning tasks"
        );
    }

    #[test]
    fn coding_task_prefers_code_model_over_generic() {
        let models = vec![
            make_model("llama3-8b", "lmstudio"),
            make_model("codestral-22b", "lmstudio"),
        ];
        let selected = select_for_task(&models, TaskKind::Coding).unwrap();
        assert_eq!(selected.id, "codestral-22b");
    }

    #[test]
    fn claude_wins_over_small_local_for_tool_use() {
        let models = vec![
            make_model("tiny-llm-1b", "lmstudio"),
            make_model("claude-sonnet-4-6", "claude"),
        ];
        let selected = select_for_task(&models, TaskKind::ToolUse).unwrap();
        assert_eq!(selected.backend, "claude");
    }

    #[test]
    fn qwen_beats_generic_chat_model_for_tools() {
        let models = vec![
            make_model("mistral-7b", "lmstudio"),
            make_model("qwen/qwen3-14b", "lmstudio"),
        ];
        let selected = select_for_task(&models, TaskKind::ToolUse).unwrap();
        assert_eq!(selected.id, "qwen/qwen3-14b");
    }

    #[test]
    fn reasoning_model_still_selected_when_only_option() {
        let models = vec![make_model("deepseek-r1-14b", "lmstudio")];
        // Even for ToolUse — no other choice.
        let selected = select_for_task(&models, TaskKind::ToolUse).unwrap();
        assert_eq!(selected.id, "deepseek-r1-14b");
    }

    // ── TaskKind round-trip ───────────────────────────────────────────────────

    #[test]
    fn task_kind_parse_round_trip() {
        for kind in [
            TaskKind::Coding,
            TaskKind::Reasoning,
            TaskKind::ToolUse,
            TaskKind::Analysis,
            TaskKind::Chat,
        ] {
            assert_eq!(TaskKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn task_kind_parse_unknown_returns_none() {
        assert_eq!(TaskKind::parse("unknown_task"), None);
    }

    // ── to_config_model ───────────────────────────────────────────────────────

    #[test]
    fn claude_backend_maps_to_claude_model() {
        let d = make_model("claude-sonnet-4-6", "claude");
        let m = to_config_model(&d);
        assert!(matches!(m, contract::config::Model::Claude(ref s) if s == "claude-sonnet-4-6"));
    }

    #[test]
    fn lmstudio_backend_maps_to_lmstudio_model() {
        let d = make_model("qwen/qwen3-8b", "lmstudio");
        let m = to_config_model(&d);
        assert!(
            matches!(m, contract::config::Model::LMStudio { ref id, .. } if id == "qwen/qwen3-8b")
        );
    }

    #[test]
    fn ollama_backend_maps_to_ollama_model() {
        let d = make_model("some-model", "ollama");
        let m = to_config_model(&d);
        assert!(matches!(m, contract::config::Model::Ollama { ref id, .. } if id == "some-model"));
    }
}
