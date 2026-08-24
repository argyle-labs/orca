//! `model.*` tool surface — installed-model registry. See
//! [[project-model-agent-conversation-ownership]]: agents USE models;
//! they don't own them. A model row pairs a provider (anthropic,
//! lmstudio, ollama, claude-code) with an endpoint + a specific model
//! name. Exactly one row may be marked `is_default`. The Anthropic API
//! key lives in `secrets` under `model.<id>.api_key`.

use crate::discovery::discover_all;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelRow {
    pub id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub model_name: String,
    pub is_default: bool,
    pub enabled: bool,
    pub created_at: String,
    /// True when an API key is stored for this model. The key value is
    /// never returned; callers see only presence.
    pub api_key_in_db: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_masked: Option<String>,
}

fn key_name(id: &str) -> String {
    format!("model.{id}.api_key")
}

fn enrich(conn: &db::Conn, m: db::models::Model) -> anyhow::Result<ModelRow> {
    let stored = db::settings::secret_get(conn, &key_name(&m.id))?;
    let api_key_masked = stored.as_deref().map(db::settings::mask_key);
    Ok(ModelRow {
        id: m.id,
        provider: m.provider,
        endpoint: m.endpoint,
        model_name: m.model_name,
        is_default: m.is_default,
        enabled: m.enabled,
        created_at: m.created_at,
        api_key_in_db: stored.is_some(),
        api_key_masked,
    })
}

fn validate_provider(provider: &str, endpoint: Option<&str>) -> anyhow::Result<()> {
    match provider {
        "anthropic" | "claude-code" => {
            if endpoint.is_some() {
                anyhow::bail!("provider '{provider}' does not take an endpoint");
            }
        }
        "lmstudio" | "ollama" => {
            if endpoint.is_none_or(str::is_empty) {
                anyhow::bail!("provider '{provider}' requires endpoint URL");
            }
        }
        other => anyhow::bail!(
            "unknown provider '{other}' (want: anthropic|lmstudio|ollama|claude-code)"
        ),
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// model.list
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct ModelListArgs {
    /// Filter by provider.
    #[arg(long)]
    pub provider: Option<String>,
    /// Only enabled rows.
    #[arg(long)]
    pub enabled_only: bool,
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ModelListOutput {
    pub models: Vec<ModelRow>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// List installed models (filter by provider / enabled) — THIN + paginated.
#[orca_tool(domain = "model", verb = "list")]
async fn model_list(
    args: ModelListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ModelListOutput> {
    let conn = db::open_default()?;
    let rows = db::models::list(&conn)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(p) = args.provider.as_deref()
            && row.provider != p
        {
            continue;
        }
        if args.enabled_only && !row.enabled {
            continue;
        }
        out.push(enrich(&conn, row)?);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(out, &params);
    Ok(ModelListOutput {
        models: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// model.detail
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ModelDetailArgs {
    pub id: String,
}

/// Show one installed model.
#[orca_tool(domain = "model", verb = "detail")]
async fn model_detail(args: ModelDetailArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<ModelRow> {
    let conn = db::open_default()?;
    let row = db::models::get(&conn, &args.id)?
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found", args.id))?;
    enrich(&conn, row)
}

// ═══════════════════════════════════════════════════════════════════════════
// model.create — install a new model
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelCreateArgs {
    /// User-chosen id, unique. Examples: "claude-opus", "local-llama3-70b".
    pub id: String,
    /// One of: anthropic, lmstudio, ollama, claude-code.
    #[arg(long)]
    pub provider: String,
    /// Required for lmstudio/ollama; rejected for anthropic/claude-code.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Provider-specific model name (e.g. "claude-opus-4-7", "llama3:70b").
    /// Empty allowed for claude-code (no upstream model).
    #[arg(long, default_value = "")]
    pub model_name: String,
    /// Mark this row the global default. Clears any previous default.
    #[arg(long)]
    pub is_default: bool,
    /// API key stored in the encrypted orca DB. Only meaningful for
    /// `anthropic`; ignored for local providers and claude-code.
    #[arg(long)]
    pub api_key: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ModelCreateOutput {
    pub id: String,
}

/// [MUTATES STATE] Install a new model. Errors if `id` already exists.
#[orca_tool(domain = "model", verb = "create")]
async fn model_create(
    args: ModelCreateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ModelCreateOutput> {
    validate_provider(&args.provider, args.endpoint.as_deref())?;
    let mut conn = db::open_default()?;
    if db::models::exists(&conn, &args.id)? {
        anyhow::bail!(
            "model '{}' already exists; use model.update to modify",
            args.id
        );
    }
    let row = db::models::Model {
        id: args.id.clone(),
        provider: args.provider,
        endpoint: args.endpoint,
        model_name: args.model_name,
        is_default: args.is_default,
        enabled: true,
        created_at: String::new(),
    };
    db::models::insert(&mut conn, &row)?;
    if let Some(key) = args.api_key.as_deref() {
        if key.trim().is_empty() {
            anyhow::bail!("api_key must not be empty");
        }
        db::settings::secret_set(&conn, &key_name(&args.id), key)?;
    }
    Ok(ModelCreateOutput { id: args.id })
}

// ═══════════════════════════════════════════════════════════════════════════
// model.update — modify an existing model
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelUpdateArgs {
    pub id: String,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub endpoint: Option<String>,
    #[arg(long)]
    pub model_name: Option<String>,
    #[arg(long)]
    pub is_default: Option<bool>,
    #[arg(long)]
    pub enabled: Option<bool>,
    /// Replace the stored API key.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Drop the stored API key.
    #[arg(long)]
    pub clear_api_key: bool,
}

/// [MUTATES STATE] Modify an existing model. Errors if `id` is unknown;
/// use `model.create` to install one.
#[orca_tool(domain = "model", verb = "update")]
async fn model_update(args: ModelUpdateArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<ModelRow> {
    let mut conn = db::open_default()?;
    let mut row = db::models::get(&conn, &args.id)?.ok_or_else(|| {
        anyhow::anyhow!("model '{}' not found; use model.create to install", args.id)
    })?;
    if let Some(p) = args.provider {
        row.provider = p;
    }
    // endpoint: explicit Some("") means clear; None means leave alone
    if let Some(e) = args.endpoint {
        row.endpoint = if e.is_empty() { None } else { Some(e) };
    }
    if let Some(n) = args.model_name {
        row.model_name = n;
    }
    if let Some(d) = args.is_default {
        row.is_default = d;
    }
    if let Some(en) = args.enabled {
        row.enabled = en;
    }
    validate_provider(&row.provider, row.endpoint.as_deref())?;
    db::models::update(&mut conn, &row)?;

    if args.clear_api_key {
        db::settings::secret_delete(&conn, &key_name(&args.id))?;
    }
    if let Some(key) = args.api_key.as_deref() {
        if key.trim().is_empty() {
            anyhow::bail!("api_key must not be empty");
        }
        db::settings::secret_set(&conn, &key_name(&args.id), key)?;
    }
    enrich(&conn, row)
}

// ═══════════════════════════════════════════════════════════════════════════
// model.delete
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ModelDeleteArgs {
    pub id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ModelDeleteOutput {
    pub id: String,
    pub deleted: bool,
}

/// [MUTATES STATE] Remove a model row and its stored API key.
#[orca_tool(domain = "model", verb = "delete")]
async fn model_delete(
    args: ModelDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ModelDeleteOutput> {
    let conn = db::open_default()?;
    let deleted = db::models::remove(&conn, &args.id)?;
    if !deleted {
        anyhow::bail!("model '{}' not found", args.id);
    }
    db::settings::secret_delete(&conn, &key_name(&args.id)).ok();
    Ok(ModelDeleteOutput {
        id: args.id,
        deleted,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// model.backends_check — live reachability probe
// ═══════════════════════════════════════════════════════════════════════════
//
// Complements `model.list` (which lists *registered* DB rows): this reports
// which backends are actually reachable right now and what models they serve,
// via `discover_all`. Moved into core from the retired `llm` plugin.

/// One probed backend and the models it currently serves.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct BackendStatus {
    /// Backend kind: "anthropic" / "lmstudio" / "ollama".
    pub backend: String,
    /// Base URL probed (empty for the Anthropic API).
    pub url: String,
    /// Whether at least one usable (non-embedding) model was discovered.
    pub reachable: bool,
    /// Model identifiers discovered on this backend right now.
    pub models: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct BackendsCheckOutput {
    /// One entry per distinct backend endpoint discovered.
    pub backends: Vec<BackendStatus>,
    /// Total count of usable models across all reachable backends.
    pub total_models: u32,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct BackendsCheckArgs {}

/// Probe every configured LLM backend (DB-registered providers + the
/// `LMSTUDIO_URL` / `OLLAMA_URL` env defaults + the Anthropic API if a key is
/// configured) and report which are reachable and what they serve right now.
/// Availability is dynamic, so this reflects live state at call time, not
/// stored configuration.
#[orca_tool(domain = "model", verb = "backends_check")]
async fn model_backends_check(
    _args: BackendsCheckArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<BackendsCheckOutput> {
    let discovered = discover_all(&ctx.config).await;

    // Group discovered models by (backend, url) endpoint.
    let mut grouped: Vec<BackendStatus> = Vec::new();
    for m in &discovered {
        if let Some(existing) = grouped
            .iter_mut()
            .find(|b| b.backend == m.backend && b.url == m.url)
        {
            existing.models.push(m.id.clone());
        } else {
            grouped.push(BackendStatus {
                backend: m.backend.clone(),
                url: m.url.clone(),
                reachable: true,
                models: vec![m.id.clone()],
            });
        }
    }

    let total_models = grouped.iter().map(|b| b.models.len() as u32).sum();
    Ok(BackendsCheckOutput {
        backends: grouped,
        total_models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── key_name ──────────────────────────────────────────────────────────────

    #[test]
    fn key_name_wraps_id_in_secret_path() {
        assert_eq!(key_name("claude-opus"), "model.claude-opus.api_key");
        assert_eq!(key_name(""), "model..api_key");
    }

    // ── validate_provider ─────────────────────────────────────────────────────

    #[test]
    fn validate_provider_anthropic_rejects_endpoint() {
        assert!(validate_provider("anthropic", None).is_ok());
        let err = validate_provider("anthropic", Some("http://x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not take an endpoint"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
    }

    #[test]
    fn validate_provider_claude_code_rejects_endpoint() {
        assert!(validate_provider("claude-code", None).is_ok());
        assert!(validate_provider("claude-code", Some("http://x")).is_err());
    }

    #[test]
    fn validate_provider_ollama_requires_endpoint() {
        assert!(validate_provider("ollama", Some("http://localhost:11434")).is_ok());
        let err = validate_provider("ollama", None).unwrap_err().to_string();
        assert!(err.contains("requires endpoint URL"), "{err}");
        let err = validate_provider("ollama", Some(""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires endpoint URL"), "{err}");
    }

    #[test]
    fn validate_provider_lmstudio_requires_endpoint() {
        assert!(validate_provider("lmstudio", Some("http://localhost:1234")).is_ok());
        assert!(validate_provider("lmstudio", None).is_err());
        assert!(validate_provider("lmstudio", Some("")).is_err());
    }

    #[test]
    fn validate_provider_unknown_reports_value_and_options() {
        let err = validate_provider("gemini", None).unwrap_err().to_string();
        assert!(err.contains("unknown provider 'gemini'"), "{err}");
        assert!(
            err.contains("anthropic|lmstudio|ollama|claude-code"),
            "{err}"
        );
    }

    // ── ModelRow serde (camelCase, option skipping) ───────────────────────────

    fn full_row() -> ModelRow {
        ModelRow {
            id: "m1".into(),
            provider: "anthropic".into(),
            endpoint: Some("http://x".into()),
            model_name: "claude".into(),
            is_default: true,
            enabled: true,
            created_at: "2021-01-01T00:00:00Z".into(),
            api_key_in_db: true,
            api_key_masked: Some("sk-…abcd".into()),
        }
    }

    #[test]
    fn model_row_serializes_camel_case_with_all_fields() {
        let s = serde_json::to_string(&full_row()).unwrap();
        assert!(s.contains("\"modelName\":\"claude\""), "{s}");
        assert!(s.contains("\"isDefault\":true"), "{s}");
        assert!(s.contains("\"createdAt\":\"2021-01-01T00:00:00Z\""), "{s}");
        assert!(s.contains("\"apiKeyInDb\":true"), "{s}");
        assert!(s.contains("\"apiKeyMasked\":\"sk-…abcd\""), "{s}");
        assert!(s.contains("\"endpoint\":\"http://x\""), "{s}");
    }

    #[test]
    fn model_row_omits_none_endpoint_and_masked_key() {
        let row = ModelRow {
            endpoint: None,
            api_key_masked: None,
            api_key_in_db: false,
            ..full_row()
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(
            !s.contains("endpoint"),
            "None endpoint must be omitted: {s}"
        );
        assert!(
            !s.contains("apiKeyMasked"),
            "None masked key must be omitted: {s}"
        );
        assert!(s.contains("\"apiKeyInDb\":false"), "{s}");
    }

    // ── ModelListArgs serde (default container) ───────────────────────────────

    #[test]
    fn model_list_args_default_is_empty() {
        let a = ModelListArgs::default();
        assert!(a.provider.is_none());
        assert!(!a.enabled_only);
        assert!(a.limit.is_none());
        assert!(a.cursor.is_none());
    }

    #[test]
    fn model_list_args_empty_object_uses_defaults() {
        let a: ModelListArgs = serde_json::from_str("{}").unwrap();
        assert!(a.provider.is_none());
        assert!(!a.enabled_only);
    }

    #[test]
    fn model_list_args_parses_fields() {
        let a: ModelListArgs = serde_json::from_str(
            "{\"provider\":\"ollama\",\"enabled_only\":true,\"limit\":10,\"cursor\":\"c\"}",
        )
        .unwrap();
        assert_eq!(a.provider.as_deref(), Some("ollama"));
        assert!(a.enabled_only);
        assert_eq!(a.limit, Some(10));
        assert_eq!(a.cursor.as_deref(), Some("c"));
    }

    // ── ModelListOutput serde (option skipping) ───────────────────────────────

    #[test]
    fn model_list_output_omits_none_cursor_and_total() {
        let s = serde_json::to_string(&ModelListOutput {
            models: vec![],
            next_cursor: None,
            total: None,
        })
        .unwrap();
        assert_eq!(s, "{\"models\":[]}");
    }

    #[test]
    fn model_list_output_includes_present_cursor_and_total() {
        let s = serde_json::to_string(&ModelListOutput {
            models: vec![],
            next_cursor: Some("nc".into()),
            total: Some(9),
        })
        .unwrap();
        assert!(s.contains("\"next_cursor\":\"nc\""), "{s}");
        assert!(s.contains("\"total\":9"), "{s}");
    }

    // ── ModelCreateArgs / Output serde ────────────────────────────────────────

    #[test]
    fn model_create_args_minimal_option_fields_default_to_none() {
        // Option fields are absent → None (serde's built-in Option handling);
        // is_default (bool) and model_name (String) must be supplied.
        let a: ModelCreateArgs = serde_json::from_str(
            "{\"id\":\"m1\",\"provider\":\"claude-code\",\"modelName\":\"\",\"isDefault\":false}",
        )
        .unwrap();
        assert_eq!(a.id, "m1");
        assert_eq!(a.provider, "claude-code");
        assert_eq!(a.model_name, "");
        assert!(!a.is_default);
        assert!(a.endpoint.is_none());
        assert!(a.api_key.is_none());
    }

    #[test]
    fn model_create_args_camel_case_round_trip() {
        let a: ModelCreateArgs = serde_json::from_str(
            "{\"id\":\"m1\",\"provider\":\"ollama\",\"endpoint\":\"http://h\",\"modelName\":\"llama3\",\"isDefault\":true,\"apiKey\":\"k\"}",
        )
        .unwrap();
        assert_eq!(a.model_name, "llama3");
        assert!(a.is_default);
        assert_eq!(a.endpoint.as_deref(), Some("http://h"));
        assert_eq!(a.api_key.as_deref(), Some("k"));
    }

    #[test]
    fn model_create_output_serializes_id() {
        let s = serde_json::to_string(&ModelCreateOutput { id: "m1".into() }).unwrap();
        assert_eq!(s, "{\"id\":\"m1\"}");
    }

    // ── ModelUpdateArgs serde ─────────────────────────────────────────────────

    #[test]
    fn model_update_args_all_optional() {
        // Option fields absent → None; clear_api_key (bool) must be supplied.
        let a: ModelUpdateArgs =
            serde_json::from_str("{\"id\":\"m1\",\"clearApiKey\":false}").unwrap();
        assert_eq!(a.id, "m1");
        assert!(a.provider.is_none());
        assert!(a.endpoint.is_none());
        assert!(a.model_name.is_none());
        assert!(a.is_default.is_none());
        assert!(a.enabled.is_none());
        assert!(a.api_key.is_none());
        assert!(!a.clear_api_key);
    }

    #[test]
    fn model_update_args_camel_case_fields() {
        let a: ModelUpdateArgs = serde_json::from_str(
            "{\"id\":\"m1\",\"modelName\":\"n\",\"isDefault\":false,\"enabled\":true,\"clearApiKey\":true}",
        )
        .unwrap();
        assert_eq!(a.model_name.as_deref(), Some("n"));
        assert_eq!(a.is_default, Some(false));
        assert_eq!(a.enabled, Some(true));
        assert!(a.clear_api_key);
    }

    // ── ModelDeleteArgs / Output serde ────────────────────────────────────────

    #[test]
    fn model_delete_args_and_output_round_trip() {
        let a: ModelDeleteArgs = serde_json::from_str("{\"id\":\"m1\"}").unwrap();
        assert_eq!(a.id, "m1");
        let s = serde_json::to_string(&ModelDeleteOutput {
            id: "m1".into(),
            deleted: true,
        })
        .unwrap();
        assert_eq!(s, "{\"id\":\"m1\",\"deleted\":true}");
    }

    // ── BackendStatus / BackendsCheckOutput serde ─────────────────────────────

    #[test]
    fn backend_status_serializes_all_fields() {
        let s = serde_json::to_string(&BackendStatus {
            backend: "ollama".into(),
            url: "http://localhost:11434".into(),
            reachable: true,
            models: vec!["llama3".into(), "qwen3".into()],
        })
        .unwrap();
        assert!(s.contains("\"backend\":\"ollama\""), "{s}");
        assert!(s.contains("\"reachable\":true"), "{s}");
        assert!(s.contains("\"models\":[\"llama3\",\"qwen3\"]"), "{s}");
    }

    #[test]
    fn backends_check_output_round_trips() {
        let out = BackendsCheckOutput {
            backends: vec![BackendStatus {
                backend: "lmstudio".into(),
                url: "http://x".into(),
                reachable: true,
                models: vec!["m".into()],
            }],
            total_models: 1,
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("\"total_models\":1"), "{s}");
        let back: BackendsCheckOutput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.total_models, 1);
        assert_eq!(back.backends.len(), 1);
        assert_eq!(back.backends[0].backend, "lmstudio");
    }

    #[test]
    fn backends_check_args_empty_object() {
        let _a: BackendsCheckArgs = serde_json::from_str("{}").unwrap();
        let _d = BackendsCheckArgs::default();
    }

    // ── Tool-body integration against an ephemeral db ─────────────────────────
    //
    // The `#[orca_tool]` inner fns call `db::open_default()`, which honors the
    // task-local override set by `db::with_db_path` (pointing at an unencrypted
    // tempfile). This exercises `enrich`, the mutation paths, and the api-key
    // secret plumbing without touching the canonical store.

    use std::sync::Arc;

    /// Extract an error message without requiring the `Ok` type to be `Debug`
    /// (the tool output rows don't derive `Debug`, so `Result::unwrap_err`
    /// isn't available on them).
    fn err_str<T>(r: anyhow::Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    fn test_ctx() -> contract::ToolCtx {
        use contract::config::{Config, Model};
        use std::path::PathBuf;
        let cfg = Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/unused.db"),
            ports: Default::default(),
        };
        contract::ToolCtx::new(Arc::new(cfg))
    }

    fn create_args(id: &str, provider: &str, endpoint: Option<&str>) -> ModelCreateArgs {
        ModelCreateArgs {
            id: id.into(),
            provider: provider.into(),
            endpoint: endpoint.map(String::from),
            model_name: "m".into(),
            is_default: false,
            api_key: None,
        }
    }

    async fn with_db<F, Fut, T>(f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("models.db");
        db::with_db_path(path, async move { f().await }).await
    }

    #[tokio::test]
    async fn create_then_detail_and_list_roundtrip() {
        with_db(|| async {
            let ctx = test_ctx();
            let out = model_create(create_args("m1", "ollama", Some("http://h")), &ctx)
                .await
                .unwrap();
            assert_eq!(out.id, "m1");

            let row = model_detail(ModelDetailArgs { id: "m1".into() }, &ctx)
                .await
                .unwrap();
            assert_eq!(row.provider, "ollama");
            assert_eq!(row.endpoint.as_deref(), Some("http://h"));
            assert!(row.enabled);
            assert!(!row.api_key_in_db);

            let listed = model_list(ModelListArgs::default(), &ctx).await.unwrap();
            assert_eq!(listed.models.len(), 1);
            assert_eq!(listed.models[0].id, "m1");
            assert_eq!(listed.total, Some(1));
        })
        .await;
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        with_db(|| async {
            let ctx = test_ctx();
            model_create(create_args("dup", "ollama", Some("http://h")), &ctx)
                .await
                .unwrap();
            let err =
                err_str(model_create(create_args("dup", "ollama", Some("http://h")), &ctx).await);
            assert!(err.contains("already exists"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn create_rejects_invalid_provider_before_db_write() {
        with_db(|| async {
            let ctx = test_ctx();
            let err = err_str(model_create(create_args("bad", "gemini", None), &ctx).await);
            assert!(err.contains("unknown provider"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn create_with_empty_api_key_errors() {
        with_db(|| async {
            let ctx = test_ctx();
            let mut args = create_args("k", "anthropic", None);
            args.api_key = Some("   ".into());
            let err = err_str(model_create(args, &ctx).await);
            assert!(err.contains("api_key must not be empty"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn create_with_api_key_marks_present_and_masked() {
        with_db(|| async {
            let ctx = test_ctx();
            let mut args = create_args("ak", "anthropic", None);
            args.api_key = Some("sk-supersecretvalue".into());
            model_create(args, &ctx).await.unwrap();

            let row = model_detail(ModelDetailArgs { id: "ak".into() }, &ctx)
                .await
                .unwrap();
            assert!(row.api_key_in_db);
            let masked = row.api_key_masked.unwrap();
            assert!(!masked.contains("supersecret"), "key must be masked");
        })
        .await;
    }

    #[tokio::test]
    async fn detail_unknown_id_errors() {
        with_db(|| async {
            let ctx = test_ctx();
            let err = err_str(model_detail(ModelDetailArgs { id: "nope".into() }, &ctx).await);
            assert!(err.contains("not found"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn list_filters_by_provider_and_enabled() {
        with_db(|| async {
            let ctx = test_ctx();
            model_create(create_args("a", "ollama", Some("http://h")), &ctx)
                .await
                .unwrap();
            model_create(create_args("b", "anthropic", None), &ctx)
                .await
                .unwrap();
            // disable b
            model_update(
                ModelUpdateArgs {
                    id: "b".into(),
                    provider: None,
                    endpoint: None,
                    model_name: None,
                    is_default: None,
                    enabled: Some(false),
                    api_key: None,
                    clear_api_key: false,
                },
                &ctx,
            )
            .await
            .unwrap();

            let only_ollama = model_list(
                ModelListArgs {
                    provider: Some("ollama".into()),
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(only_ollama.models.len(), 1);
            assert_eq!(only_ollama.models[0].id, "a");

            let enabled = model_list(
                ModelListArgs {
                    enabled_only: true,
                    ..Default::default()
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(enabled.models.len(), 1);
            assert_eq!(enabled.models[0].id, "a");
        })
        .await;
    }

    #[tokio::test]
    async fn update_unknown_id_errors() {
        with_db(|| async {
            let ctx = test_ctx();
            let err = err_str(
                model_update(
                    ModelUpdateArgs {
                        id: "ghost".into(),
                        provider: None,
                        endpoint: None,
                        model_name: None,
                        is_default: None,
                        enabled: None,
                        api_key: None,
                        clear_api_key: false,
                    },
                    &ctx,
                )
                .await,
            );
            assert!(err.contains("not found"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn update_changes_fields_and_clears_endpoint() {
        with_db(|| async {
            let ctx = test_ctx();
            model_create(create_args("u", "ollama", Some("http://old")), &ctx)
                .await
                .unwrap();
            let row = model_update(
                ModelUpdateArgs {
                    id: "u".into(),
                    provider: Some("anthropic".into()),
                    endpoint: Some(String::new()), // explicit clear
                    model_name: Some("claude".into()),
                    is_default: Some(true),
                    enabled: Some(true),
                    api_key: None,
                    clear_api_key: false,
                },
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(row.provider, "anthropic");
            assert!(row.endpoint.is_none());
            assert_eq!(row.model_name, "claude");
            assert!(row.is_default);
        })
        .await;
    }

    #[tokio::test]
    async fn update_empty_api_key_errors() {
        with_db(|| async {
            let ctx = test_ctx();
            model_create(create_args("uk", "anthropic", None), &ctx)
                .await
                .unwrap();
            let err = err_str(
                model_update(
                    ModelUpdateArgs {
                        id: "uk".into(),
                        provider: None,
                        endpoint: None,
                        model_name: None,
                        is_default: None,
                        enabled: None,
                        api_key: Some("  ".into()),
                        clear_api_key: false,
                    },
                    &ctx,
                )
                .await,
            );
            assert!(err.contains("api_key must not be empty"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn update_clear_api_key_drops_stored_key() {
        with_db(|| async {
            let ctx = test_ctx();
            let mut args = create_args("ck", "anthropic", None);
            args.api_key = Some("sk-value".into());
            model_create(args, &ctx).await.unwrap();

            let row = model_update(
                ModelUpdateArgs {
                    id: "ck".into(),
                    provider: None,
                    endpoint: None,
                    model_name: None,
                    is_default: None,
                    enabled: None,
                    api_key: None,
                    clear_api_key: true,
                },
                &ctx,
            )
            .await
            .unwrap();
            assert!(!row.api_key_in_db);
            assert!(row.api_key_masked.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn delete_removes_row_then_errors_on_missing() {
        with_db(|| async {
            let ctx = test_ctx();
            model_create(create_args("d", "ollama", Some("http://h")), &ctx)
                .await
                .unwrap();
            let out = model_delete(ModelDeleteArgs { id: "d".into() }, &ctx)
                .await
                .unwrap();
            assert!(out.deleted);
            assert_eq!(out.id, "d");

            let err = err_str(model_delete(ModelDeleteArgs { id: "d".into() }, &ctx).await);
            assert!(err.contains("not found"), "{err}");
        })
        .await;
    }
}
