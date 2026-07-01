//! `model.*` tool surface — installed-model registry. See
//! [[project-model-agent-conversation-ownership]]: agents USE models;
//! they don't own them. A model row pairs a provider (anthropic,
//! lmstudio, ollama, claude-code) with an endpoint + a specific model
//! name. Exactly one row may be marked `is_default`. The Anthropic API
//! key lives in `secrets` under `model.<id>.api_key`.

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
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ModelListOutput {
    pub models: Vec<ModelRow>,
}

/// List installed models (filter by provider / enabled).
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
    Ok(ModelListOutput { models: out })
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
