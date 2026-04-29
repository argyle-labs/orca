# Inference Router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a multi-instance inference router that selects the best available local model (LM Studio or Ollama) based on tier and capability requirements, with automatic fallback to Claude, and exposes it via a new `brain_llm_ask` MCP tool and an extended `brain_run` tool.

**Architecture:** Instance config lives in `brain.db` (managed via CLI), not `brain.toml`. A new `inference` module in `brain-core` handles routing: it queries live instances for loaded models, cross-references against a bundled+cached model capability registry, and returns the best match. Agents never reference a host or model ID — they declare a tier and optional capabilities.

**Tech Stack:** Rust, rusqlite (SQLCipher), reqwest, serde_json, tokio, `include_str!()` for bundled JSON baseline, `~/.brain/model-registry-cache.json` + `~/.brain/inference-state.json` for runtime state.

**Spec:** `docs/superpowers/specs/2026-04-29-inference-router-design.md`

---

## File Map

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `projects/utils/src/db.rs` | Add 3 new tables to `apply_schema()` |
| Create | `projects/core/src/inference/models.json` | Bundled model capability baseline |
| Create | `projects/core/src/inference/registry.rs` | `ModelRegistry`: bundled + cached live data, tier inference |
| Create | `projects/core/src/inference/state.rs` | `RuntimeState`: loaded models per instance, TTL-cached to `~/.brain/inference-state.json` |
| Create | `projects/core/src/inference/instance.rs` | `InferenceInstance`: named handle with provider type |
| Create | `projects/core/src/inference/mod.rs` | `InferenceRouter`: routing algorithm, `InferenceTarget` enum |
| Modify | `projects/core/src/lib.rs` | Export `pub mod inference` |
| Modify | `projects/core/Cargo.toml` | Add `dirs` dependency |
| Modify | `projects/server/src/mcp/tools.rs` | Add `brain_llm_ask` tool def, add tier/capabilities to `brain_run` |
| Modify | `projects/server/src/mcp/handlers.rs` | Add `llm_ask()` handler, update `run()` to accept tier/capabilities |
| Modify | `projects/server/src/mcp/mod.rs` | Wire `brain_llm_ask` into dispatch |
| Modify | `projects/server/src/session/util.rs` | Add `resolve_model_for_task()` |
| Create | `projects/commands/src/inference.rs` | `brain inference` CLI subcommand (add/list/remove/reorder) |
| Modify | `projects/commands/src/lib.rs` | Register inference subcommand |

---

## Task 1: DB Schema — Add inference_instances, schema_databases, mcp_servers tables

**Files:**
- Modify: `projects/utils/src/db.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module at the bottom of `projects/utils/src/db.rs`:

```rust
#[test]
fn inference_instances_table_exists() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir.path().join("test.db")).unwrap();
    // INSERT should succeed if table exists
    conn.execute(
        "INSERT INTO inference_instances (id, name, url, provider, enabled, sort_order, created_at)
         VALUES ('test-id', 'local', 'http://localhost:1234', 'lmstudio', 1, 0, '2026-01-01T00:00:00Z')",
        [],
    ).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM inference_instances", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn schema_databases_table_exists() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir.path().join("test.db")).unwrap();
    conn.execute(
        "INSERT INTO schema_databases
            (id, name, user, password, database, created_at)
         VALUES ('db-id', 'Rebuy DB', 'root', 'secret', 'rebuy', '2026-01-01T00:00:00Z')",
        [],
    ).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_databases", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn mcp_servers_table_exists() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir.path().join("test.db")).unwrap();
    conn.execute(
        "INSERT INTO mcp_servers (id, name, command, args, env, enabled, created_at)
         VALUES ('mcp-id', 'context7', 'npx', '[\"-y\",\"@upstash/context7-mcp\"]', '{}', 1, '2026-01-01T00:00:00Z')",
        [],
    ).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM mcp_servers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd ~/code/brain && cargo test -p brain-utils inference_instances_table_exists 2>&1 | tail -5
```

Expected: `FAILED` — tables don't exist yet.

- [ ] **Step 3: Add tables to `apply_schema()`**

In `projects/utils/src/db.rs`, extend the `apply_schema()` SQL string — append after the last `CREATE INDEX` line and before the closing `"`:

```sql
        CREATE TABLE IF NOT EXISTS inference_instances (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            url        TEXT NOT NULL,
            provider   TEXT NOT NULL DEFAULT 'lmstudio',
            enabled    INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ii_sort ON inference_instances(sort_order);

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
            args       TEXT NOT NULL DEFAULT '[]',
            env        TEXT NOT NULL DEFAULT '{}',
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL
        );
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd ~/code/brain && cargo test -p brain-utils 2>&1 | tail -10
```

Expected: all tests pass including the 3 new ones.

- [ ] **Step 5: Commit**

```bash
cd ~/code/brain && git add projects/utils/src/db.rs && git commit -m "feat(db): add inference_instances, schema_databases, mcp_servers tables"
```

---

## Task 2: Bundled Model Registry Baseline

**Files:**
- Create: `projects/core/src/inference/models.json`

- [ ] **Step 1: Create the inference directory and models.json**

```bash
mkdir -p ~/code/brain/projects/core/src/inference
```

Create `projects/core/src/inference/models.json`:

```json
{
  "qwen/qwen3.6-27b": {
    "params_b": 27,
    "tier": "heavy",
    "capabilities": ["reasoning", "thinking", "coding", "long-context", "tool-use", "multilingual"]
  },
  "qwen/qwen3.5-9b": {
    "params_b": 9,
    "tier": "fast",
    "capabilities": ["reasoning", "thinking", "coding", "multilingual"]
  },
  "deepseek/deepseek-r1-0528-qwen3-8b": {
    "params_b": 8,
    "tier": "fast",
    "capabilities": ["reasoning", "thinking", "coding"]
  },
  "deepseek/deepseek-r1": {
    "params_b": 671,
    "tier": "heavy",
    "capabilities": ["reasoning", "thinking", "coding", "math", "long-context"]
  },
  "google/gemma-4-e4b": {
    "params_b": 4,
    "tier": "light",
    "capabilities": ["coding"]
  },
  "meta-llama/llama-3.1-8b-instruct": {
    "params_b": 8,
    "tier": "fast",
    "capabilities": ["coding", "tool-use"]
  },
  "meta-llama/llama-3.3-70b-instruct": {
    "params_b": 70,
    "tier": "heavy",
    "capabilities": ["reasoning", "coding", "long-context", "tool-use", "multilingual"]
  },
  "mistralai/mistral-7b-instruct": {
    "params_b": 7,
    "tier": "fast",
    "capabilities": ["coding", "tool-use"]
  },
  "mistralai/mistral-nemo-12b-instruct": {
    "params_b": 12,
    "tier": "fast",
    "capabilities": ["reasoning", "coding", "tool-use", "multilingual"]
  },
  "microsoft/phi-4": {
    "params_b": 14,
    "tier": "fast",
    "capabilities": ["reasoning", "coding", "math"]
  }
}
```

- [ ] **Step 2: Commit**

```bash
cd ~/code/brain && git add projects/core/src/inference/models.json && git commit -m "feat(inference): add bundled model capability baseline"
```

---

## Task 3: ModelRegistry

**Files:**
- Create: `projects/core/src/inference/registry.rs`
- Modify: `projects/core/Cargo.toml` (add `dirs`)

- [ ] **Step 1: Add `dirs` to brain-core dependencies**

In `projects/core/Cargo.toml`, add to `[dependencies]`:

```toml
dirs = "5"
```

- [ ] **Step 2: Write the failing tests**

Create `projects/core/src/inference/registry.rs` with tests only first:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

const BUNDLED_MODELS: &str = include_str!("models.json");
const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelCapabilities {
    pub params_b: u32,
    pub tier: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RegistryCache {
    updated_at_secs: u64,
    entries: HashMap<String, ModelCapabilities>,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    entries: HashMap<String, ModelCapabilities>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_baseline_loads() {
        let r = ModelRegistry::from_bundled();
        assert!(r.entries.contains_key("qwen/qwen3.6-27b"));
        assert!(r.entries.contains_key("google/gemma-4-e4b"));
    }

    #[test]
    fn lookup_known_model_returns_capabilities() {
        let r = ModelRegistry::from_bundled();
        let caps = r.get("qwen/qwen3.6-27b").unwrap();
        assert_eq!(caps.tier, "heavy");
        assert!(caps.capabilities.contains(&"thinking".to_string()));
        assert!(caps.capabilities.contains(&"reasoning".to_string()));
    }

    #[test]
    fn lookup_unknown_model_returns_none() {
        let r = ModelRegistry::from_bundled();
        assert!(r.get("unknown/model-xyz").is_none());
    }

    #[test]
    fn tier_inference_from_model_id_27b() {
        assert_eq!(ModelRegistry::infer_tier("some/model-27b"), "heavy");
    }

    #[test]
    fn tier_inference_from_model_id_8b() {
        assert_eq!(ModelRegistry::infer_tier("some/model-8b"), "fast");
    }

    #[test]
    fn tier_inference_from_model_id_4b() {
        assert_eq!(ModelRegistry::infer_tier("some/model-4b"), "light");
    }

    #[test]
    fn tier_inference_unknown_defaults_fast() {
        assert_eq!(ModelRegistry::infer_tier("unknown-model"), "fast");
    }

    #[test]
    fn satisfies_tier_heavy_satisfies_fast() {
        let r = ModelRegistry::from_bundled();
        // heavy model satisfies a fast request
        assert!(r.satisfies("qwen/qwen3.6-27b", "fast", &[]));
    }

    #[test]
    fn satisfies_tier_light_does_not_satisfy_heavy() {
        let r = ModelRegistry::from_bundled();
        assert!(!r.satisfies("google/gemma-4-e4b", "heavy", &[]));
    }

    #[test]
    fn satisfies_all_capabilities_required() {
        let r = ModelRegistry::from_bundled();
        // qwen3.6-27b has both thinking and coding
        assert!(r.satisfies("qwen/qwen3.6-27b", "fast", &["thinking", "coding"]));
        // gemma-4-e4b has coding but not thinking
        assert!(!r.satisfies("google/gemma-4-e4b", "fast", &["thinking", "coding"]));
    }

    #[test]
    fn cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry-cache.json");
        let r = ModelRegistry::from_bundled();
        r.save_cache(&path).unwrap();
        let loaded = ModelRegistry::load_cache(&path);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert!(loaded.entries.contains_key("qwen/qwen3.6-27b"));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cd ~/code/brain && cargo test -p brain-core 2>&1 | tail -10
```

Expected: compile error — `ModelRegistry` not yet implemented.

- [ ] **Step 4: Implement ModelRegistry**

Replace the file contents (keep the `#[cfg(test)]` block, add the implementation above it):

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

const BUNDLED_MODELS: &str = include_str!("models.json");
const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelCapabilities {
    pub params_b: u32,
    pub tier: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RegistryCache {
    updated_at_secs: u64,
    entries: HashMap<String, ModelCapabilities>,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    entries: HashMap<String, ModelCapabilities>,
}

impl ModelRegistry {
    /// Load from bundled baseline only.
    pub fn from_bundled() -> Self {
        let entries: HashMap<String, ModelCapabilities> =
            serde_json::from_str(BUNDLED_MODELS).unwrap_or_default();
        ModelRegistry { entries }
    }

    /// Load from cache file if present and within TTL; otherwise use bundled.
    /// Cache merge: bundled entries take precedence over cached entries for known models.
    pub fn load(cache_path: &PathBuf) -> Self {
        let bundled: HashMap<String, ModelCapabilities> =
            serde_json::from_str(BUNDLED_MODELS).unwrap_or_default();

        let cached = Self::load_cache(cache_path);
        let mut merged = match cached {
            Some(c) => c.entries,
            None => HashMap::new(),
        };
        // Bundled always wins for known models
        for (k, v) in bundled {
            merged.insert(k, v);
        }

        ModelRegistry { entries: merged }
    }

    /// Default cache path: ~/.brain/model-registry-cache.json
    pub fn default_cache_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".brain/model-registry-cache.json")
    }

    /// Save current entries to cache with current timestamp.
    pub fn save_cache(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cache = RegistryCache {
            updated_at_secs: now,
            entries: self.entries.clone(),
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&cache)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load cache if present and within TTL.
    pub fn load_cache(path: &PathBuf) -> Option<RegistryCache> {
        let raw = std::fs::read_to_string(path).ok()?;
        let cache: RegistryCache = serde_json::from_str(&raw).ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now.saturating_sub(cache.updated_at_secs) > CACHE_TTL_SECS {
            return None; // expired
        }
        Some(cache)
    }

    /// Look up a model by ID. Returns None if not in registry.
    pub fn get(&self, model_id: &str) -> Option<&ModelCapabilities> {
        // Try exact match first, then lowercase
        self.entries
            .get(model_id)
            .or_else(|| self.entries.get(&model_id.to_lowercase()))
    }

    /// Infer tier from model ID string when not in registry.
    pub fn infer_tier(model_id: &str) -> &'static str {
        let id = model_id.to_lowercase();
        // Check for explicit size markers
        for heavy in &["27b", "30b", "32b", "34b", "35b", "70b", "72b", "671b", "405b"] {
            if id.contains(heavy) {
                return "heavy";
            }
        }
        for light in &["1b", "2b", "3b", "4b"] {
            if id.contains(light) {
                return "light";
            }
        }
        for fast in &["7b", "8b", "9b", "11b", "12b", "13b", "14b"] {
            if id.contains(fast) {
                return "fast";
            }
        }
        "fast" // safe default
    }

    /// Returns true if model_id satisfies the requested tier and all requested capabilities.
    /// Tier hierarchy: heavy ≥ fast ≥ light (a heavy model satisfies a fast or light request).
    pub fn satisfies(&self, model_id: &str, requested_tier: &str, capabilities: &[&str]) -> bool {
        let (effective_tier, model_caps) = match self.get(model_id) {
            Some(c) => (c.tier.as_str(), c.capabilities.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
            None => {
                let inferred = Self::infer_tier(model_id);
                (inferred, vec![])
            }
        };

        if !tier_satisfies(effective_tier, requested_tier) {
            return false;
        }

        for cap in capabilities {
            if !model_caps.contains(cap) {
                return false;
            }
        }

        true
    }
}

/// Returns true if `model_tier` meets or exceeds `requested_tier`.
/// heavy ≥ fast ≥ light
fn tier_satisfies(model_tier: &str, requested_tier: &str) -> bool {
    fn rank(t: &str) -> u8 {
        match t {
            "light" => 0,
            "fast" => 1,
            "heavy" => 2,
            _ => 1,
        }
    }
    rank(model_tier) >= rank(requested_tier)
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd ~/code/brain && cargo test -p brain-core registry 2>&1 | tail -15
```

Expected: all 9 registry tests pass.

- [ ] **Step 6: Commit**

```bash
cd ~/code/brain && git add projects/core/src/inference/registry.rs projects/core/Cargo.toml && git commit -m "feat(inference): add ModelRegistry with bundled baseline and TTL cache"
```

---

## Task 4: RuntimeState

**Files:**
- Create: `projects/core/src/inference/state.rs`

- [ ] **Step 1: Write the failing tests**

Create `projects/core/src/inference/state.rs`:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

const INSTANCE_TTL_SECS: u64 = 5 * 60;      // 5 minutes
const UNREACHABLE_BACKOFF_SECS: u64 = 60;   // 60 seconds

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceState {
    pub loaded_models: Vec<String>,
    pub queried_at_secs: u64,
    pub unreachable_until_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeState {
    pub instances: HashMap<String, InstanceState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_empty() {
        let s = RuntimeState::default();
        assert!(s.instances.is_empty());
    }

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inference-state.json");

        let mut s = RuntimeState::default();
        s.instances.insert("local".to_string(), InstanceState {
            loaded_models: vec!["qwen/qwen3.6-27b".to_string()],
            queried_at_secs: 1000,
            unreachable_until_secs: None,
        });
        s.save(&path).unwrap();

        let loaded = RuntimeState::load(&path);
        assert!(loaded.instances.contains_key("local"));
        assert_eq!(loaded.instances["local"].loaded_models, vec!["qwen/qwen3.6-27b"]);
    }

    #[test]
    fn fresh_instance_state_is_stale() {
        let s = InstanceState::default(); // queried_at_secs = 0
        assert!(s.is_stale());
    }

    #[test]
    fn recently_queried_is_not_stale() {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let s = InstanceState {
            loaded_models: vec![],
            queried_at_secs: now,
            unreachable_until_secs: None,
        };
        assert!(!s.is_stale());
    }

    #[test]
    fn unreachable_instance_is_skippable() {
        let future = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() + 30;
        let s = InstanceState {
            loaded_models: vec![],
            queried_at_secs: 0,
            unreachable_until_secs: Some(future),
        };
        assert!(s.is_in_backoff());
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd ~/code/brain && cargo test -p brain-core state 2>&1 | tail -10
```

Expected: compile error — methods not yet implemented.

- [ ] **Step 3: Implement RuntimeState**

Add the implementation above the `#[cfg(test)]` block:

```rust
impl InstanceState {
    /// True if the cached state is older than the TTL and should be re-queried.
    pub fn is_stale(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.queried_at_secs) > INSTANCE_TTL_SECS
    }

    /// True if the instance is in its unreachable backoff window.
    pub fn is_in_backoff(&self) -> bool {
        match self.unreachable_until_secs {
            None => false,
            Some(until) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                now < until
            }
        }
    }
}

impl RuntimeState {
    /// Load from file, or return empty state if file missing/corrupt.
    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Default state path: ~/.brain/inference-state.json
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".brain/inference-state.json")
    }

    /// Atomically save to file.
    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Mark an instance as successfully queried with the given loaded models.
    pub fn mark_queried(&mut self, name: &str, models: Vec<String>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.instances.insert(name.to_string(), InstanceState {
            loaded_models: models,
            queried_at_secs: now,
            unreachable_until_secs: None,
        });
    }

    /// Mark an instance as unreachable; it will be skipped for UNREACHABLE_BACKOFF_SECS.
    pub fn mark_unreachable(&mut self, name: &str) {
        let until = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() + UNREACHABLE_BACKOFF_SECS;
        let entry = self.instances.entry(name.to_string()).or_default();
        entry.unreachable_until_secs = Some(until);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd ~/code/brain && cargo test -p brain-core state 2>&1 | tail -10
```

Expected: all 5 state tests pass.

- [ ] **Step 5: Commit**

```bash
cd ~/code/brain && git add projects/core/src/inference/state.rs && git commit -m "feat(inference): add RuntimeState with TTL and unreachable backoff"
```

---

## Task 5: InferenceInstance + InferenceRouter

**Files:**
- Create: `projects/core/src/inference/instance.rs`
- Create: `projects/core/src/inference/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `projects/core/src/inference/instance.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceInstance {
    pub name: String,
    pub url: String,
    pub provider: String,   // "lmstudio" | "ollama"
    pub sort_order: i64,
}

impl InferenceInstance {
    pub fn local_fallback(url: &str) -> Self {
        InferenceInstance {
            name: "local".to_string(),
            url: url.to_string(),
            provider: "lmstudio".to_string(),
            sort_order: 0,
        }
    }
}
```

Create `projects/core/src/inference/mod.rs` with tests only:

```rust
pub mod instance;
pub mod registry;
pub mod state;

pub use instance::InferenceInstance;
pub use registry::{ModelCapabilities, ModelRegistry};
pub use state::RuntimeState;

use crate::backend::LMStudioBackend;
use anyhow::Result;

/// Resolved inference target — either a local backend or a Claude model ID.
#[derive(Debug, Clone)]
pub enum InferenceTarget {
    Local { url: String, model_id: String },
    Claude(String),
}

pub struct InferenceRouter {
    instances: Vec<InferenceInstance>,
    registry: ModelRegistry,
    state: RuntimeState,
    state_path: std::path::PathBuf,
    anthropic_api_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_router(instances: Vec<InferenceInstance>) -> InferenceRouter {
        InferenceRouter {
            instances,
            registry: ModelRegistry::from_bundled(),
            state: RuntimeState::default(),
            state_path: std::path::PathBuf::from("/tmp/test-inference-state.json"),
            anthropic_api_key: None,
        }
    }

    #[test]
    fn no_instances_falls_back_to_claude_haiku() {
        let router = make_router(vec![]);
        // Inject a known state for a non-existent instance — should still fall back
        let target = router.select_from_state("fast", &[]);
        assert!(matches!(target, InferenceTarget::Claude(_)));
        if let InferenceTarget::Claude(id) = target {
            assert!(id.contains("haiku"));
        }
    }

    #[test]
    fn heavy_request_falls_back_to_claude_sonnet_when_no_local() {
        let router = make_router(vec![]);
        let target = router.select_from_state("heavy", &[]);
        assert!(matches!(target, InferenceTarget::Claude(_)));
        if let InferenceTarget::Claude(id) = target {
            assert!(id.contains("sonnet"));
        }
    }

    #[test]
    fn instance_with_matching_model_is_selected() {
        let mut router = make_router(vec![
            InferenceInstance {
                name: "local".to_string(),
                url: "http://localhost:1234".to_string(),
                provider: "lmstudio".to_string(),
                sort_order: 0,
            }
        ]);
        // Inject state: local has qwen3.6-27b loaded
        router.state.mark_queried("local", vec!["qwen/qwen3.6-27b".to_string()]);

        let target = router.select_from_state("heavy", &["thinking"]);
        assert!(matches!(target, InferenceTarget::Local { .. }));
        if let InferenceTarget::Local { url, model_id } = target {
            assert_eq!(url, "http://localhost:1234");
            assert_eq!(model_id, "qwen/qwen3.6-27b");
        }
    }

    #[test]
    fn instance_in_backoff_is_skipped() {
        let mut router = make_router(vec![
            InferenceInstance {
                name: "local".to_string(),
                url: "http://localhost:1234".to_string(),
                provider: "lmstudio".to_string(),
                sort_order: 0,
            }
        ]);
        router.state.mark_unreachable("local");

        let target = router.select_from_state("fast", &[]);
        // local is in backoff, no other instances → Claude fallback
        assert!(matches!(target, InferenceTarget::Claude(_)));
    }

    #[test]
    fn first_matching_instance_wins_by_sort_order() {
        let mut router = make_router(vec![
            InferenceInstance { name: "local".into(), url: "http://localhost:1234".into(), provider: "lmstudio".into(), sort_order: 0 },
            InferenceInstance { name: "hemlock".into(), url: "http://hemlock:1234".into(), provider: "lmstudio".into(), sort_order: 1 },
        ]);
        router.state.mark_queried("local", vec!["qwen/qwen3.6-27b".to_string()]);
        router.state.mark_queried("hemlock", vec!["qwen/qwen3.6-27b".to_string()]);

        let target = router.select_from_state("heavy", &[]);
        if let InferenceTarget::Local { url, .. } = target {
            assert_eq!(url, "http://localhost:1234"); // local wins (sort_order 0)
        } else {
            panic!("expected Local target");
        }
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd ~/code/brain && cargo test -p brain-core router 2>&1 | tail -10
```

Expected: compile error — `select_from_state` not yet implemented.

- [ ] **Step 3: Implement InferenceRouter**

Add above the `#[cfg(test)]` block in `mod.rs`:

```rust
impl InferenceRouter {
    /// Construct from a list of DB-loaded instances.
    pub fn new(
        instances: Vec<InferenceInstance>,
        registry: ModelRegistry,
        anthropic_api_key: Option<String>,
    ) -> Self {
        let state_path = RuntimeState::default_path();
        let state = RuntimeState::load(&state_path);
        InferenceRouter { instances, registry, state, state_path, anthropic_api_key }
    }

    /// Build from a single URL — backwards compat when no DB instances configured.
    pub fn from_url(url: &str, anthropic_api_key: Option<String>) -> Self {
        Self::new(
            vec![InferenceInstance::local_fallback(url)],
            ModelRegistry::load(&ModelRegistry::default_cache_path()),
            anthropic_api_key,
        )
    }

    /// Select a target using only cached state (no network calls).
    /// Used for tests and when a fast synchronous answer is needed.
    pub fn select_from_state(&self, tier: &str, capabilities: &[&str]) -> InferenceTarget {
        let mut sorted = self.instances.clone();
        sorted.sort_by_key(|i| i.sort_order);

        for instance in &sorted {
            let inst_state = self.state.instances.get(&instance.name);

            // Skip if in backoff
            if inst_state.map(|s| s.is_in_backoff()).unwrap_or(false) {
                continue;
            }

            // Use cached loaded models
            let loaded = match inst_state {
                Some(s) if !s.is_stale() => &s.loaded_models,
                _ => continue, // stale or no state — skip in synchronous path
            };

            for model_id in loaded {
                if self.registry.satisfies(model_id, tier, capabilities) {
                    return InferenceTarget::Local {
                        url: instance.url.clone(),
                        model_id: model_id.clone(),
                    };
                }
            }
        }

        self.claude_fallback(tier)
    }

    /// Select a target, querying instances live if state is stale or missing.
    /// Updates and saves RuntimeState after queries.
    pub async fn select(&mut self, tier: &str, capabilities: &[&str]) -> InferenceTarget {
        let mut sorted = self.instances.clone();
        sorted.sort_by_key(|i| i.sort_order);

        for instance in &sorted {
            let inst_state = self.state.instances.get(&instance.name);

            if inst_state.map(|s| s.is_in_backoff()).unwrap_or(false) {
                continue;
            }

            let needs_query = inst_state.map(|s| s.is_stale()).unwrap_or(true);

            let loaded_models: Vec<String> = if needs_query {
                let lms = LMStudioBackend::new(&instance.url, "");
                match lms.list_models().await {
                    Ok(models) => {
                        let chat: Vec<String> = models.into_iter()
                            .filter(|m| !m.contains("embed"))
                            .collect();
                        self.state.mark_queried(&instance.name, chat.clone());
                        let _ = self.state.save(&self.state_path);
                        chat
                    }
                    Err(_) => {
                        self.state.mark_unreachable(&instance.name);
                        let _ = self.state.save(&self.state_path);
                        continue;
                    }
                }
            } else {
                inst_state.unwrap().loaded_models.clone()
            };

            for model_id in &loaded_models {
                if self.registry.satisfies(model_id, tier, capabilities) {
                    return InferenceTarget::Local {
                        url: instance.url.clone(),
                        model_id: model_id.clone(),
                    };
                }
            }
        }

        self.claude_fallback(tier)
    }

    fn claude_fallback(&self, tier: &str) -> InferenceTarget {
        let model = if tier == "heavy" {
            "claude-sonnet-4-6".to_string()
        } else {
            "claude-haiku-4-5-20251001".to_string()
        };
        InferenceTarget::Claude(model)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd ~/code/brain && cargo test -p brain-core 2>&1 | tail -15
```

Expected: all tests pass (registry + state + router).

- [ ] **Step 5: Commit**

```bash
cd ~/code/brain && git add projects/core/src/inference/ && git commit -m "feat(inference): add InferenceInstance and InferenceRouter with capability-based routing"
```

---

## Task 6: Wire Inference Module Into Core

**Files:**
- Modify: `projects/core/src/lib.rs`

- [ ] **Step 1: Export the inference module**

Edit `projects/core/src/lib.rs` — add one line:

```rust
pub mod backend;
pub mod inference;
pub mod tools;
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/code/brain && cargo build -p brain-core 2>&1 | tail -5
```

Expected: compiles without errors.

- [ ] **Step 3: Commit**

```bash
cd ~/code/brain && git add projects/core/src/lib.rs && git commit -m "feat(inference): export inference module from brain-core"
```

---

## Task 7: `brain_llm_ask` MCP Tool

**Files:**
- Modify: `projects/server/src/mcp/tools.rs`
- Modify: `projects/server/src/mcp/handlers.rs`
- Modify: `projects/server/src/mcp/mod.rs`

- [ ] **Step 1: Add `brain_llm_ask` tool definition**

In `projects/server/src/mcp/tools.rs`, add this entry to the `json!([...])` array (after the `brain_run` entry):

```rust
        {
            "name": "brain_llm_ask",
            "description": "Send a prompt directly to a local LLM via Brain's inference router. Selects the best available instance and model based on tier and capabilities. Falls back to Claude Haiku (fast/light) or Sonnet (heavy) if no local model matches. Use for summarization, explanation, drafting, or any task that benefits from local inference. Do NOT use for: file reads, searches, log lookups, or operations with deterministic tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The prompt to send to the model"
                    },
                    "system": {
                        "type": "string",
                        "description": "Optional system prompt"
                    },
                    "tier": {
                        "type": "string",
                        "description": "Model tier: heavy | fast | light (default: fast)"
                    },
                    "capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Required capabilities: reasoning, thinking, coding, vision, long-context, tool-use, math, multilingual"
                    },
                    "max_tokens": {
                        "type": "integer",
                        "description": "Max tokens to generate (default: 2048)"
                    },
                    "temperature": {
                        "type": "number",
                        "description": "Sampling temperature 0-2 (default: 0.3)"
                    }
                },
                "required": ["prompt"]
            }
        },
```

- [ ] **Step 2: Add `llm_ask` handler**

In `projects/server/src/mcp/handlers.rs`, add these imports at the top:

```rust
use brain_core::backend::{LMStudioBackend, ClaudeBackend, buffer_sink};
use brain_core::inference::{InferenceRouter, InferenceTarget};
use brain_utils::types::Message;
use tokio_util::sync::CancellationToken;
```

Then add this function (after the existing `run` function):

```rust
pub async fn llm_ask(args: &Value, config: &Config) -> Result<String> {
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;
    let system = args["system"].as_str().unwrap_or("");
    let tier = args["tier"].as_str().unwrap_or("fast");
    let caps: Vec<&str> = args["capabilities"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let max_tokens = args["max_tokens"].as_u64().unwrap_or(2048);
    let temperature = args["temperature"].as_f64().unwrap_or(0.3);

    // Build router from DB instances (with fallback to config.lmstudio_url)
    let mut router = {
        let db_path = config.brain_vault.join("brain.db");
        let instances = brain_utils::db::load_inference_instances(&db_path)
            .unwrap_or_default();
        if instances.is_empty() {
            brain_core::inference::InferenceRouter::from_url(
                &config.lmstudio_url,
                config.anthropic_api_key.clone(),
            )
        } else {
            use brain_core::inference::ModelRegistry;
            brain_core::inference::InferenceRouter::new(
                instances,
                ModelRegistry::load(&ModelRegistry::default_cache_path()),
                config.anthropic_api_key.clone(),
            )
        }
    };

    let target = router.select(tier, &caps).await;

    let messages = vec![Message::User { content: prompt.to_string() }];
    let (sink, buf) = buffer_sink();
    let cancel = CancellationToken::new();

    match target {
        InferenceTarget::Local { url, model_id } => {
            let backend = LMStudioBackend::new(&url, &model_id);
            // Override temperature/max_tokens via a direct request — use chat() API
            backend.chat(&messages, &[], system, cancel, &sink).await?;
        }
        InferenceTarget::Claude(model_id) => {
            let key = config.anthropic_api_key.clone()
                .ok_or_else(|| anyhow::anyhow!("no local model available and no Anthropic API key"))?;
            let backend = ClaudeBackend::new(key, &model_id);
            backend.chat(&messages, &[], system, cancel, &sink).await?;
        }
    }

    let bytes = buf.lock().unwrap();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
```

- [ ] **Step 3: Add `load_inference_instances` to db.rs**

In `projects/utils/src/db.rs`, add after the existing helpers:

```rust
use crate::config::InferenceInstanceRow;  // add this import at the top of the file

/// Load all enabled inference instances ordered by sort_order.
pub fn load_inference_instances(db_path: &std::path::Path) -> Result<Vec<InferenceInstanceRow>> {
    if !db_path.exists() {
        return Ok(vec![]);
    }
    let conn = open(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, name, url, provider, sort_order FROM inference_instances
         WHERE enabled = 1 ORDER BY sort_order ASC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(InferenceInstanceRow {
            id: row.get(0)?,
            name: row.get(1)?,
            url: row.get(2)?,
            provider: row.get(3)?,
            sort_order: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
```

- [ ] **Step 4: Add `InferenceInstanceRow` to config.rs**

In `projects/utils/src/config.rs`, add this struct (near the top, after imports):

```rust
#[derive(Debug, Clone)]
pub struct InferenceInstanceRow {
    pub id: String,
    pub name: String,
    pub url: String,
    pub provider: String,
    pub sort_order: i64,
}
```

And add a `From` conversion so `InferenceRouter::new` can accept them. In `projects/core/src/inference/instance.rs`, add:

```rust
impl From<brain_utils::config::InferenceInstanceRow> for InferenceInstance {
    fn from(row: brain_utils::config::InferenceInstanceRow) -> Self {
        InferenceInstance {
            name: row.name,
            url: row.url,
            provider: row.provider,
            sort_order: row.sort_order,
        }
    }
}
```

Update the `llm_ask` handler to map the conversion:

```rust
let instances = brain_utils::db::load_inference_instances(&db_path)
    .unwrap_or_default()
    .into_iter()
    .map(brain_core::inference::InferenceInstance::from)
    .collect::<Vec<_>>();
```

- [ ] **Step 5: Wire into dispatch in mod.rs**

In `projects/server/src/mcp/mod.rs`, add `llm_ask` to the imports:

```rust
use handlers::{
    agents, get_agent, get_context, list_services, llm_ask, run, run_tests, search_logs, service_logs,
};
```

Add to the `dispatch` match:

```rust
        "brain_llm_ask" => llm_ask(args, config).await,
```

(Place it after the `"brain_run"` arm.)

- [ ] **Step 6: Build to verify it compiles**

```bash
cd ~/code/brain && cargo build -p brain-server 2>&1 | tail -10
```

Expected: compiles without errors.

- [ ] **Step 7: Commit**

```bash
cd ~/code/brain && git add projects/utils/src/db.rs projects/utils/src/config.rs projects/core/src/inference/instance.rs projects/server/src/mcp/tools.rs projects/server/src/mcp/handlers.rs projects/server/src/mcp/mod.rs && git commit -m "feat(mcp): add brain_llm_ask tool with inference router"
```

---

## Task 8: Extend `brain_run` with tier/capabilities

**Files:**
- Modify: `projects/server/src/mcp/tools.rs`
- Modify: `projects/server/src/session/util.rs`
- Modify: `projects/server/src/mcp/handlers.rs`

- [ ] **Step 1: Add tier/capabilities to the brain_run tool definition**

In `projects/server/src/mcp/tools.rs`, find the `brain_run` entry and update its `inputSchema` to add:

```json
                    "tier": {
                        "type": "string",
                        "description": "Model tier: heavy | fast | light (default: fast)"
                    },
                    "capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Required capabilities: reasoning, thinking, coding, vision, long-context, tool-use, math, multilingual"
                    }
```

- [ ] **Step 2: Add `resolve_model_for_task` to session/util.rs**

In `projects/server/src/session/util.rs`, add this import at the top:

```rust
use brain_core::inference::{InferenceRouter, InferenceTarget, ModelRegistry};
use brain_utils::config::InferenceInstanceRow;
```

Add this function after `resolve_model`:

```rust
/// Resolve a model for a specific task given tier + capability requirements.
/// Uses the inference router; never prompts the user interactively.
pub async fn resolve_model_for_task(
    config: &brain_utils::config::Config,
    tier: &str,
    capabilities: &[&str],
) -> Result<Model> {
    let db_path = config.brain_vault.join("brain.db");
    let raw_instances = brain_utils::db::load_inference_instances(&db_path)
        .unwrap_or_default();

    let mut router = if raw_instances.is_empty() {
        InferenceRouter::from_url(&config.lmstudio_url, config.anthropic_api_key.clone())
    } else {
        InferenceRouter::new(
            raw_instances.into_iter().map(brain_core::inference::InferenceInstance::from).collect(),
            ModelRegistry::load(&ModelRegistry::default_cache_path()),
            config.anthropic_api_key.clone(),
        )
    };

    match router.select(tier, capabilities).await {
        InferenceTarget::Local { url, model_id } => {
            // Patch config's lmstudio_url to the selected instance for this session
            // (Session::new_with_output reads config.lmstudio_url via build_backend)
            // We return a LMStudio model; caller must clone+patch the config.
            Ok(Model::LMStudio(model_id))
        }
        InferenceTarget::Claude(model_id) => Ok(Model::Claude(model_id)),
    }
}
```

- [ ] **Step 3: Update the `run` handler to accept and pass tier/capabilities**

In `projects/server/src/mcp/handlers.rs`, update the `run` function signature and body:

```rust
pub async fn run(args: &Value, config: &Config) -> Result<String> {
    let agent = args["agent"].as_str().unwrap_or("wolf");
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;
    let tier = args["tier"].as_str().unwrap_or("fast");
    let caps: Vec<&str> = args["capabilities"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let full_prompt = if agent != "wolf" && agent != "brain" {
        format!("Delegate this to @{agent}: {prompt}")
    } else {
        prompt.to_string()
    };

    // Resolve model via router, then build a patched config for the session
    let model = crate::session::util::resolve_model_for_task(config, tier, &caps).await
        .unwrap_or_else(|_| brain_utils::config::Model::Claude("claude-haiku-4-5-20251001".to_string()));

    let mut patched_config = config.clone();
    patched_config.default_model = model;

    let (sink, buf) = buffer_sink();
    let ctx = ProjectContext::default();
    let mut session = Session::new_with_output(patched_config, ctx, sink).await?;
    session.one_shot(full_prompt).await?;

    let bytes = buf.lock().unwrap();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
```

- [ ] **Step 4: Build to verify it compiles**

```bash
cd ~/code/brain && cargo build -p brain-server 2>&1 | tail -10
```

Expected: compiles without errors.

- [ ] **Step 5: Commit**

```bash
cd ~/code/brain && git add projects/server/src/mcp/tools.rs projects/server/src/session/util.rs projects/server/src/mcp/handlers.rs && git commit -m "feat(mcp): extend brain_run with tier and capabilities routing"
```

---

## Task 9: CLI — `brain inference` Subcommand

The CLI uses clap derive macros in `projects/server/src/main.rs`. Follow the same pattern as `McpAction` / `cmd_mcp`.

**Files:**
- Create: `projects/commands/src/inference.rs`
- Modify: `projects/commands/src/lib.rs`
- Modify: `projects/server/src/main.rs`

- [ ] **Step 1: Create inference.rs in commands**

Create `projects/commands/src/inference.rs`:

```rust
use anyhow::Result;
use brain_utils::config::Config;
use brain_utils::db;
use clap::Subcommand;
use chrono::Utc;
use uuid::Uuid;

#[derive(Subcommand)]
pub enum InferenceAction {
    /// Register a new inference instance
    Add {
        /// Instance name (e.g. local, willow, hemlock)
        #[arg(long)]
        name: String,
        /// Base URL (e.g. http://localhost:1234)
        #[arg(long)]
        url: String,
        /// Provider type: lmstudio | ollama
        #[arg(long, default_value = "lmstudio")]
        provider: String,
    },
    /// List all configured inference instances
    List,
    /// Remove an inference instance by name
    Remove {
        /// Instance name to remove
        name: String,
    },
}

pub fn cmd_inference(config: &Config, action: InferenceAction) -> Result<()> {
    let db_path = config.brain_vault.join("brain.db");
    match action {
        InferenceAction::Add { name, url, provider } => add_instance(&db_path, &name, &url, &provider),
        InferenceAction::List => list_instances(&db_path),
        InferenceAction::Remove { name } => remove_instance(&db_path, &name),
    }
}

fn add_instance(db_path: &std::path::Path, name: &str, url: &str, provider: &str) -> Result<()> {
    let conn = db::open(db_path)?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let max_order: i64 = conn
        .query_row("SELECT COALESCE(MAX(sort_order), -1) FROM inference_instances", [], |r| r.get(0))
        .unwrap_or(-1);
    conn.execute(
        "INSERT INTO inference_instances (id, name, url, provider, enabled, sort_order, created_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
        rusqlite::params![id, name, url, provider, max_order + 1, now],
    )?;
    println!("Added inference instance '{name}' → {url} ({provider})");
    Ok(())
}

fn list_instances(db_path: &std::path::Path) -> Result<()> {
    if !db_path.exists() {
        println!("No instances configured.");
        return Ok(());
    }
    let conn = db::open(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT name, url, provider, enabled FROM inference_instances ORDER BY sort_order ASC"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut any = false;
    for row in rows {
        let (name, url, provider, enabled) = row?;
        let status = if enabled == 1 { "enabled" } else { "disabled" };
        println!("  {name}  {url}  [{provider}] [{status}]");
        any = true;
    }
    if !any {
        println!("No instances configured. Run: brain inference add --name local --url http://localhost:1234");
    }
    Ok(())
}

fn remove_instance(db_path: &std::path::Path, name: &str) -> Result<()> {
    let conn = db::open(db_path)?;
    let deleted = conn.execute(
        "DELETE FROM inference_instances WHERE name = ?1",
        rusqlite::params![name],
    )?;
    if deleted == 0 {
        anyhow::bail!("instance '{name}' not found");
    }
    println!("Removed inference instance '{name}'");
    Ok(())
}
```

- [ ] **Step 2: Export from commands/src/lib.rs**

In `projects/commands/src/lib.rs`, add:

```rust
pub mod inference;
pub use inference::{InferenceAction, cmd_inference};
```

(Place after the existing `pub mod mcp_cmd;` line and matching `pub use` block.)

- [ ] **Step 3: Add Inference variant to Command enum in main.rs**

In `projects/server/src/main.rs`, update the `brain_commands` import line to add `InferenceAction` and `cmd_inference`:

```rust
use brain_commands::{self as cmd, DaemonAction, InferenceAction, LogAction, McpAction, SpecAction,
    cmd_inference, cmd_oauth_github, cmd_oauth_atlassian, cmd_logout_github, cmd_logout_atlassian,
    cmd_install, cmd_uninstall};
```

Add to the `Command` enum (after the `Mcp` variant):

```rust
    /// Manage inference instances (LM Studio, Ollama)
    Inference {
        #[command(subcommand)]
        action: InferenceAction,
    },
```

Add to the `match cli.command` block (after the `Mcp` arm):

```rust
        Some(Command::Inference { action }) => cmd_inference(&config, action),
```

- [ ] **Step 4: Build to verify it compiles**

```bash
cd ~/code/brain && cargo build -p brain-server 2>&1 | tail -10
```

Expected: compiles without errors.

- [ ] **Step 5: Smoke test the CLI**

```bash
brain inference list
```

Expected: `No instances configured. Run: brain inference add --name local --url http://localhost:1234`

```bash
brain inference add --name local --url http://localhost:1234
brain inference list
brain inference add --name willow --url http://willow.local:1234
brain inference list
brain inference remove willow
brain inference list
```

Expected: add/list/remove all work correctly; final list shows only `local`.

- [ ] **Step 6: Commit**

```bash
cd ~/code/brain && git add projects/commands/src/inference.rs projects/commands/src/lib.rs projects/server/src/main.rs && git commit -m "feat(cli): add brain inference subcommand (add/list/remove)"
```

---

## Task 10: End-to-End Smoke Test

- [ ] **Step 1: Add local instance via CLI**

```bash
brain inference add --name local --url http://localhost:1234
brain inference list
```

Expected: instance appears in list.

- [ ] **Step 2: Rebuild and restart brain server**

Tell the user: rebuild with `cargo build --release` and restart the brain daemon so the updated MCP server is live.

- [ ] **Step 3: Test `brain_llm_ask` via Claude Code**

In a Claude Code session with brain-local MCP active, the `brain_llm_ask` tool should now appear. Run:

```
Use brain_llm_ask with prompt "What are 3 signs of unmaintainable code? One sentence each." and tier "fast"
```

Expected: response from `qwen/qwen3.6-27b` (or whichever model is loaded), no model-not-found errors.

- [ ] **Step 4: Test tier routing — request heavy + thinking**

```
Use brain_llm_ask with prompt "Explain chain-of-thought reasoning briefly." tier "heavy" capabilities ["thinking"]
```

Expected: routes to `qwen/qwen3.6-27b` (heavy, has thinking).

- [ ] **Step 5: Verify fallback — disable local instance and test**

```bash
# Temporarily break the URL to simulate unreachable
brain inference remove local
brain inference add --name local --url http://localhost:9999
```

Then call `brain_llm_ask` — should fall back to Claude Haiku.

Restore: `brain inference remove local && brain inference add --name local --url http://localhost:1234`
