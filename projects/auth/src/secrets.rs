//! Secrets domain — host-level named secrets with pluggable backends.
//!
//! v1 surface: `secret.list`, `secret.get`, `secret.set`, `secret.delete`,
//! `secret.backends`. The only backend in v1 is `inline` (value stored in the
//! SQLCipher-encrypted orca.db). v2 plan adds 1Password / Bitwarden / OS
//! keychain backends as separate integration crates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use anyhow::{anyhow, bail};
#[cfg(feature = "native")]
use orca_macro::orca_tool;

// ── Shared types ────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SecretEntry {
    pub name: String,
    /// Backend kind: "inline" (v1) | "env" | "op-connect" | "op-cli" | "bitwarden" | "keychain-macos" | "secret-service" | "wincred" (v2+).
    pub backend: String,
    /// Backend-specific reference (e.g. `op://Personal/orca-gh/token`). Empty for inline.
    pub ref_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct BackendInfo {
    pub kind: String,
    pub supports_store: bool,
}

// ── secret.list ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListReport {
    pub secrets: Vec<SecretEntry>,
}

// ── secret.get ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetReport {
    pub name: String,
    pub backend: String,
    pub value: String,
}

// ── secret.set ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SecretSetArgs {
    pub name: String,
    /// Backend kind. Defaults to "inline".
    #[serde(default = "default_inline")]
    #[cfg_attr(feature = "cli", arg(long, default_value = "inline"))]
    pub backend: String,
    /// Required for `inline`. Ignored for external backends (which use `ref_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub value: Option<String>,
    /// Required for external backends (e.g. `op://Personal/orca-gh/token`). Ignored for inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub ref_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub description: Option<String>,
    /// When set, proxy the call to the named remote peer via the pod mesh
    /// instead of writing the secret locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long, hide = true))]
    pub peer_id: Option<String>,
}

fn default_inline() -> String {
    "inline".into()
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretMutationReport {
    pub name: String,
    pub backend: String,
    pub created: bool,
}

// ── secret.delete ───────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteReport {
    pub name: String,
    pub removed: bool,
}

// ── secret.backends ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendsReport {
    pub backends: Vec<BackendInfo>,
}

// ── Inline backend (v1 — value lives in encrypted DB) ───────────────────────

/// Available backend kinds on this host. v1 only knows `inline`.
#[cfg(feature = "native")]
fn known_backends() -> &'static [&'static str] {
    &["inline"]
}

/// Fetch a value by name. Returns `(backend_kind, value)`. Used by tools and by
/// internal callers (e.g. lifecycle::resolve_github_token) that need a raw secret
/// without going through `#[orca_tool]` dispatch.
#[cfg(feature = "native")]
pub async fn get_secret(name: &str) -> anyhow::Result<(String, String)> {
    let conn = db::open_default()?;
    let row =
        db::secrets::get(&conn, name)?.ok_or_else(|| anyhow!("no secret named '{name}'"))?;
    let value = match row.backend.as_str() {
        "inline" => db::secrets::read_inline_value(&conn, &row.name)?
            .ok_or_else(|| anyhow!("inline secret '{}' has no stored value", row.name))?,
        other => bail!("backend '{other}' is not supported on this host"),
    };
    Ok((row.backend, value))
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List configured secrets (names + backends + metadata). Never returns values.
#[cfg(feature = "native")]
#[orca_tool(domain = "system.secret", verb = "list")]
async fn secret_list(
    _args: SecretListArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SecretListReport> {
    let conn = db::open_default()?;
    let rows = db::secrets::list(&conn)?;
    let secrets = rows
        .into_iter()
        .map(|r| SecretEntry {
            name: r.name,
            backend: r.backend,
            ref_path: r.ref_path,
            description: r.description,
            updated_at: r.updated_at,
        })
        .collect();
    Ok(SecretListReport { secrets })
}

/// [SENSITIVE] Fetch a secret value by name. Resolves via the configured backend.
#[cfg(feature = "native")]
#[orca_tool(domain = "system.secret", verb = "detail")]
async fn secret_detail(
    args: SecretGetArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SecretGetReport> {
    let (backend, value) = get_secret(&args.name).await?;
    Ok(SecretGetReport {
        name: args.name,
        backend,
        value,
    })
}

/// [MUTATES STATE] Create or update a secret. For 'inline' backend, `value` is required;
/// for external backends, `ref_path` is required (e.g. 'op://Vault/Item/field').
/// When `peer_id` is set the secret is written on the named peer instead of locally
/// — same admin trust surface as `system.update`.
#[cfg(feature = "native")]
#[orca_tool(
    domain = "system.secret",
    verb = "set",
    remote_ok = true,
    peer_dispatch = true
)]
async fn secret_set(
    args: SecretSetArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    if !known_backends().contains(&args.backend.as_str()) {
        bail!(
            "unknown backend '{}' (available: {})",
            args.backend,
            known_backends().join(", ")
        );
    }
    match args.backend.as_str() {
        "inline" => {
            if args.value.is_none() {
                bail!("`value` is required for backend=inline");
            }
        }
        _ => {
            if args.ref_path.is_none() {
                bail!(
                    "`ref_path` is required for backend={} (e.g. 'op://Vault/Item/field')",
                    args.backend
                );
            }
        }
    }
    let ref_path_for_storage = match args.backend.as_str() {
        "inline" => String::new(),
        _ => args.ref_path.clone().unwrap(),
    };
    let conn = db::open_default()?;
    let created = db::secrets::upsert(
        &conn,
        &args.name,
        &args.backend,
        &ref_path_for_storage,
        args.description.as_deref(),
    )?;
    if args.backend == "inline" {
        db::secrets::write_inline_value(&conn, &args.name, args.value.as_deref().unwrap_or(""))?;
    }
    Ok(SecretMutationReport {
        name: args.name,
        backend: args.backend,
        created,
    })
}

/// [MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends
/// only the orca registration is removed (the upstream vault is untouched).
#[cfg(feature = "native")]
#[orca_tool(domain = "system.secret", verb = "delete")]
async fn secret_delete(
    args: SecretDeleteArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SecretDeleteReport> {
    let conn = db::open_default()?;
    let removed = db::secrets::delete(&conn, &args.name)?;
    Ok(SecretDeleteReport {
        name: args.name,
        removed,
    })
}

/// List backend kinds available on this host (lets the UI render a backend picker).
#[cfg(feature = "native")]
#[orca_tool(domain = "system.secret", verb = "backends")]
async fn secret_backends(
    _args: SecretBackendsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SecretBackendsReport> {
    // v1: inline only, supports store.
    let backends = vec![BackendInfo {
        kind: "inline".into(),
        supports_store: true,
    }];
    Ok(SecretBackendsReport { backends })
}

