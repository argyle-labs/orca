# Inference Router Design

**Date:** 2026-04-29  
**Status:** Approved  
**Scope:** Multi-instance local inference routing for Brain — LM Studio, Ollama, and remote instances (willow, hemlock)

---

## Problem

Brain currently talks to a single hardcoded LM Studio instance at `localhost:1234`. RAM constraints mean only one large model can be loaded at a time. Multiple machines (local, willow unraid server, hemlock gaming desktop) each run inference but Brain has no way to route across them, track which model is loaded, or fall back gracefully.

---

## Goals

- Route inference requests to the first available instance with the right capability
- Track currently-loaded models per instance without polling on every request
- Support LM Studio and Ollama (both speak OpenAI-compatible APIs)
- Manage instances via CLI/UI — not brain.toml
- Fall back to Claude (Haiku/Sonnet) when no local instance satisfies the request

---

## Configuration

### brain.toml — build-time only

`brain.toml` is reserved for build-time and pipeline-overridable config only. No user runtime state lives here. This allows CI/CD pipelines to override env-level settings without touching user data.

### brain.db — all runtime config

Inference instances are managed in the encrypted SQLite database (`~/brain/brain.db`) alongside schema databases and MCP servers.

**New tables added to `apply_schema()`:**

```sql
CREATE TABLE IF NOT EXISTS inference_instances (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    url        TEXT NOT NULL,
    provider   TEXT NOT NULL DEFAULT 'lmstudio',  -- 'lmstudio' | 'ollama'
    enabled    INTEGER NOT NULL DEFAULT 1,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS schema_databases (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    host         TEXT,
    port         INTEGER,
    container    TEXT,
    user         TEXT NOT NULL,
    password     TEXT NOT NULL,
    database     TEXT NOT NULL,
    domains_file TEXT,
    created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_servers (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    command    TEXT NOT NULL,
    args       TEXT NOT NULL DEFAULT '[]',  -- JSON array
    env        TEXT NOT NULL DEFAULT '{}',  -- JSON object
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL
);
```

**Note:** `[[schema.databases]]` and `[[mcp.servers]]` in `brain.toml` are migrated to these tables. See `project_config_db_migration.md` for the full migration backlog.

**CLI management:**
```sh
brain inference add --name hemlock --url http://hemlock.local:1234 --provider lmstudio
brain inference list
brain inference remove hemlock
brain inference reorder local willow hemlock
```

**Backwards compat:** If no `inference_instances` rows exist, Brain synthesizes a single local entry from the `LMSTUDIO_URL` env var (default: `http://localhost:1234`).

---

## Provider Abstraction

Both LM Studio and Ollama speak OpenAI-compatible APIs (`/v1/models`, `/v1/chat/completions`). The existing `LMStudioBackend` already implements `ModelBackend`. The `provider` field exists for future quirk-handling but does not change the wire protocol.

**New module:** `projects/core/src/inference/`

```
inference/
  mod.rs         — InferenceRouter (public entry point)
  instance.rs    — InferenceInstance: named handle wrapping LMStudioBackend
  registry.rs    — ModelRegistry: bundled baseline + live sync + cache
  state.rs       — RuntimeState: loaded models per instance, TTL-cached
```

`InferenceInstance` wraps `LMStudioBackend` with a name, provider type, and sort order sourced from the DB.

---

## Model Registry

### Bundled baseline

A `models.json` file embedded in the binary at build time via `include_str!()`. Contains common model families with capability metadata:

```json
{
  "qwen/qwen3.6-27b": {
    "params_b": 27,
    "tier": "heavy",
    "capabilities": ["reasoning", "thinking", "coding", "long-context", "tool-use", "multilingual"]
  },
  "deepseek/deepseek-r1-0528-qwen3-8b": {
    "params_b": 8,
    "tier": "fast",
    "capabilities": ["reasoning", "thinking", "coding"]
  },
  "google/gemma-4-e4b": {
    "params_b": 4,
    "tier": "light",
    "capabilities": ["coding"]
  }
}
```

### Live sync

- Fetches from upstream sources (Ollama library API, LM Studio catalog)
- Merges with bundled data — upstream entries supplement, never replace, bundled entries
- Cached to `~/.brain/model-registry-cache.json` with a **7-day TTL**
- Sync runs in the background on first request after TTL expires — never blocks a call
- Cache is invalidated and rebuilt, not patched

### Tier inference fallback

If a model ID is not in the registry, Brain infers tier from the ID string:
- Contains `27b`, `30b`, `70b`, or larger → `heavy`
- Contains `7b`, `8b`, `9b`, `13b` → `fast`
- Contains `1b`, `2b`, `3b`, `4b` → `light`
- Unknown → `fast`

---

## Routing Algorithm

Called with `tier` (default: `fast`) and optional `capabilities[]`.

1. Load `inference_instances` from DB ordered by `sort_order`
2. For each instance:
   - Check `RuntimeState` cache (5-min TTL) for loaded models
   - Cache miss or stale → query `/v1/models` live (also serves as health check)
   - Update `RuntimeState` with result
   - Cross-reference each loaded model against `ModelRegistry`
   - If a model satisfies **all** requested tier + capabilities → use it, stop scanning
3. If no local instance satisfies the request → fall back to Claude:
   - `light` or `fast` → Claude Haiku
   - `heavy` → Claude Sonnet
4. Unreachable instances are skipped and marked with a 60-second backoff before retrying

**Model swap recovery:** If a call returns a model-not-found error, Router re-queries that instance once and retries before moving on to the next instance.

---

## Capability Dimensions

Capabilities declared per model in the registry:

| Capability | Description |
|---|---|
| `reasoning` | General multi-step reasoning |
| `thinking` | Extended CoT / thinking mode (DeepSeek-R1, Qwen3, o1-style scratchpad) |
| `coding` | Code generation and analysis |
| `vision` | Multimodal image input |
| `long-context` | 128k+ context window |
| `tool-use` | Reliable function/tool calling |
| `math` | Mathematical reasoning |
| `multilingual` | Non-English language support |

Routing requires **all** requested capabilities to be present. Tier is a minimum floor — a `heavy` model satisfies a `fast` request.

---

## MCP Tool Interface

### `brain_run` (extended)

```json
{
  "name": "brain_run",
  "inputSchema": {
    "type": "object",
    "properties": {
      "agent":        { "type": "string", "description": "Agent name (e.g. wolf, owl, fox)" },
      "prompt":       { "type": "string", "description": "Task or question" },
      "tier":         { "type": "string", "description": "heavy | fast | light (default: fast)" },
      "capabilities": { "type": "array", "items": { "type": "string" }, "description": "Required model capabilities" }
    },
    "required": ["agent", "prompt"]
  }
}
```

### `brain_llm_ask` (new)

Direct inference without agent wrapping — same routing, no system prompt injection:

```json
{
  "name": "brain_llm_ask",
  "inputSchema": {
    "type": "object",
    "properties": {
      "prompt":        { "type": "string" },
      "system":        { "type": "string", "description": "Optional system prompt" },
      "tier":          { "type": "string", "description": "heavy | fast | light (default: fast)" },
      "capabilities":  { "type": "array", "items": { "type": "string" } },
      "max_tokens":    { "type": "integer", "default": 2048 },
      "temperature":   { "type": "number", "default": 0.3 }
    },
    "required": ["prompt"]
  }
}
```

Agents declare their needs; the router selects the instance and model. Agents never reference a specific host or model ID.

---

## Runtime State

**File:** `~/.brain/inference-state.json`  
**Written atomically** (temp + rename, same pattern as `state.rs`)

```json
{
  "instances": {
    "local":   { "loaded_models": ["qwen/qwen3.6-27b"], "queried_at": "2026-04-29T00:00:00Z" },
    "willow":  { "loaded_models": [], "queried_at": "2026-04-29T00:00:00Z", "unreachable_until": null },
    "hemlock": { "loaded_models": ["deepseek/deepseek-r1-0528-qwen3-8b"], "queried_at": "2026-04-29T00:00:00Z" }
  }
}
```

- Per-instance TTL: 5 minutes
- Unreachable backoff: 60 seconds
- State is advisory — a stale cache miss triggers a live query, never a hard failure

---

## Error Handling

| Condition | Behavior |
|---|---|
| Instance unreachable | Mark unreachable (60s backoff), try next instance |
| No model matches capability | Skip instance, try next |
| All local instances exhausted | Fall back to Claude Haiku or Sonnet |
| Model swapped mid-session | Re-query instance once, retry call |
| Claude also unavailable | Surface error to caller |

---

## Out of Scope

- Model loading/unloading API (LM Studio handles this when a model is requested)
- Multi-model parallel inference
- Per-instance authentication
- Load balancing across instances with identical capability
