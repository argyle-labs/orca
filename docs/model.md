# Model Subsystem

How orca selects, configures, and talks to LLMs. Covers the three backends, the model registry, discovery, per-agent pinning, and escalation.

---

## The Model enum

```rust
// projects/contract/src/config/mod.rs:105
pub enum Model {
    /// Anthropic Claude API — requires ANTHROPIC_API_KEY or a DB secret entry.
    Claude(String),
    /// LM Studio (OpenAI-compatible local server) — no API key needed.
    LMStudio { id: String, url: String },
    /// Ollama (OpenAI-compatible local/network server) — no API key needed.
    Ollama { id: String, url: String },
}
```

`Config::load()` reads environment only: `ANTHROPIC_API_KEY` (optional — also loadable from a DB secret at startup via `db::startup::load_api_key`), `LMSTUDIO_URL` (default `http://localhost:1234`), `OLLAMA_URL` (default `http://localhost:11434`).

An empty `url` on a `LMStudio`/`Ollama` model means "use the config default"; a populated `url` forces that specific endpoint.

## The three backends

`build_backend(config, model)` (`projects/model/src/backend/mod.rs:118`) constructs the right client:

| Backend | Auth | Endpoint | Notes |
|---|---|---|---|
| `ClaudeBackend` | `ANTHROPIC_API_KEY` or DB secret — errors without it | `https://api.anthropic.com/v1/messages` | `is_local() == false`; streaming |
| `LMStudioBackend` | none | `{base}/v1/chat/completions`; models via `{base}/v1/models` | 60 s stream-inactivity timeout |
| `OllamaBackend` | none | same OpenAI-compat chat; models via `{base}/api/tags` with `/v1/models` fallback | |

All three implement the `ModelBackend` trait (`backend/mod.rs:84`) — hand-desugared async methods returning `BoxFuture` (no `#[async_trait]` macro).

## The model registry (`model.*` tools)

Registered models live in the `models` DB table (`id`, `provider`, `endpoint`, `model_name`, `is_default`, `enabled`). Exactly one row can be `is_default`. API keys are stored encrypted in the settings/secrets store under `model.<id>.api_key` — never in the models row.

| Tool | What it does |
|---|---|
| `model.list` | All registered models; `--provider` filter, `--enabled_only`; shows masked key presence |
| `model.detail` | One model by id |
| `model.create` | Register a model. `--provider anthropic\|lmstudio\|ollama\|claude-code`; `--endpoint` required for local providers, rejected for anthropic/claude-code; `--api_key` stores the encrypted secret; `--is_default` promotes it |
| `model.update` | Patch any field; `--endpoint ""` clears; `--clear_api_key` deletes the stored key |
| `model.delete` | Remove the row and its stored key |
| `model.backends_check` | Live probe: groups reachable endpoints and the models they serve right now |

(All are `#[orca_tool]` fns in `projects/model/src/models.rs` — available on CLI, MCP, and HTTP `/api/v1`.)

## Resolution: which model does a session use?

`resolve_model()` (`projects/model/src/resolve.rs:123`) picks in priority order:

1. `config.default_model` if non-empty
2. the registry's `is_default` row (`db::models::default()`)
3. live discovery (`discover_all()`), scored for `TaskKind::ToolUse`

Hard-fail if nothing is reachable — there is no silent fallback.

### Per-agent pinning

An agent can be pinned to a specific registered model via the settings key `agent.<name>.model_id` (`resolve.rs:28-45`, `set_agent_model`/`get_agent_model`). Agent dispatch resolution (`resolve.rs:60-112`) maps the pinned (or default) row to one of three outcomes:

- `Resolution::Local(model)` — lmstudio/ollama, run in-process
- `Resolution::ServerClaude(model)` — anthropic provider with a stored key
- `Resolution::DelegateToClaudeCode` — `claude-code` provider: return a delegation envelope for the calling Claude Code session to execute

## Discovery

`discover_all()` (`projects/model/src/discovery.rs:212`) probes concurrently: config-default LM Studio + Ollama URLs, every enabled DB-registered provider, and Claude (when a key is configured). Embedding models are filtered out.

Each found model is classified (`classify_model`) into `ModelCapabilities`: tool support, reasoning flag, estimated context window (Claude 200k; `128k`/`32k`/`16k` name hints; default 32k), preferred `TaskKind`s, and a rank. `select_for_task(task, models)` picks the best: heavy penalty for tool-incapable models on `ToolUse`, then task-preference match, then rank.

The interactive local pick (`discover_local_llm`, `local.rs:54`) probes DB-registered providers first, then env-var/default URLs, and prefers LM Studio over Ollama. Probe timeouts: 500 ms connect / 2 s total.

## System prompt per backend

Sessions build their system prompt for the backend's capability level (`conversation/src/sessions/context.rs:61`): Claude gets the full Wolf persona (`load_agent_prompt("wolf")`); local models get a minimal direct-answer prompt — the full persona makes small models loop and narrate. The switch is literally `!backend.is_local()` at session construction.

## Escalation

- **CLI:** `orca escalate <question> [--project <name>]` — talks straight to `ClaudeBackend` with `claude-sonnet-4-6`; hard-fails without an API key.
- **In-session:** `/escalate <question>` — one-off Claude call using the current session's system prompt; tokens recorded to the ledger, tagged `escalation` in the session log. The session's model is unchanged afterward.

There is no automatic escalation policy — escalation is always explicit.
