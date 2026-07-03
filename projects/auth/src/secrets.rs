//! Secrets domain — named secrets with pluggable backends.
//!
//! Surface: `secrets.list`, `secrets.detail`, `secrets.create`,
//! `secrets.update`, `secrets.upsert`, `secrets.delete`. The three write verbs
//! keep the canonical CRUD vocabulary — `create` inserts (fails if the name
//! exists), `update` modifies an existing secret (fails if it is absent), and
//! `upsert` is the idempotent create-or-replace (HTTP PUT semantics) used for
//! rotation and automation. The only backend in v1 is `inline` (value stored in the
//! SQLCipher-encrypted orca.db). v2 plan adds 1Password / Bitwarden / OS
//! keychain backends as separate integration crates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use anyhow::{anyhow, bail};
use derive::orca_tool;

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

// ── secret.list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SecretListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretListReport {
    pub secrets: Vec<SecretEntry>,
}

// ── secret.get ──────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SecretGetArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGetReport {
    pub name: String,
    pub backend: String,
    pub value: String,
}

// ── secret write args (shared by create / update / upsert) ─────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SecretWriteArgs {
    pub name: String,
    /// Backend kind. Defaults to "inline".
    #[serde(default = "default_inline")]
    #[arg(long, default_value = "inline")]
    pub backend: String,
    /// Required for `inline`. Ignored for external backends (which use `ref_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub value: Option<String>,
    /// Required for external backends (e.g. `op://Personal/orca-gh/token`). Ignored for inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub ref_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub description: Option<String>,
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretDeleteReport {
    pub name: String,
    pub removed: bool,
}

// ── Inline backend (v1 — value lives in encrypted DB) ───────────────────────

/// Available backend kinds on this host. v1 only knows `inline`.
fn known_backends() -> &'static [&'static str] {
    &["inline"]
}

/// Fetch a value by name. Returns `(backend_kind, value)`. Used by tools and by
/// internal callers (e.g. lifecycle::resolve_github_token) that need a raw secret
/// without going through `#[orca_tool]` dispatch.
pub async fn get_secret(name: &str) -> anyhow::Result<(String, String)> {
    let conn = db::open_default()?;
    let row = db::secrets::get(&conn, name)?.ok_or_else(|| anyhow!("no secret named '{name}'"))?;
    let value = match row.backend.as_str() {
        "inline" => db::secrets::read_inline_value(&conn, &row.name)?
            .ok_or_else(|| anyhow!("inline secret '{}' has no stored value", row.name))?,
        other => bail!("backend '{other}' is not supported on this host"),
    };
    Ok((row.backend, value))
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List configured secrets (names + backends + metadata). Never returns values.
#[orca_tool(domain = "secrets", verb = "list")]
async fn secret_list(
    _args: SecretListArgs,
    _ctx: &contract::ToolCtx,
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
#[orca_tool(domain = "secrets", verb = "detail")]
async fn secret_detail(
    args: SecretGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretGetReport> {
    let (backend, value) = get_secret(&args.name).await?;
    Ok(SecretGetReport {
        name: args.name,
        backend,
        value,
    })
}

/// Which existence guard a write verb enforces before storing.
#[derive(Clone, Copy, PartialEq)]
enum WriteMode {
    /// `create` — insert only; fail if the name already exists.
    Create,
    /// `update` — modify only; fail if the name does not exist.
    Update,
    /// `upsert` — idempotent create-or-replace (HTTP PUT semantics).
    Upsert,
}

/// Enforce a write verb's existence precondition. Pure — the trust decision
/// lives here so it is unit-testable without a database. `create` refuses to
/// clobber an existing name; `update` refuses to conjure a missing one;
/// `upsert` accepts either.
fn existence_guard(mode: WriteMode, exists: bool, name: &str) -> anyhow::Result<()> {
    match mode {
        WriteMode::Create if exists => bail!(
            "secret '{name}' already exists — use `secrets.update` to change it or `secrets.upsert` to overwrite"
        ),
        WriteMode::Update if !exists => bail!(
            "no secret named '{name}' — use `secrets.create` to add it or `secrets.upsert` to create-or-replace"
        ),
        _ => Ok(()),
    }
}

/// Shared write path for `create` / `update` / `upsert`. Validates the backend +
/// required fields, enforces the mode's existence guard, then upserts the
/// metadata row and (for `inline`) the encrypted value.
async fn write_secret(
    args: SecretWriteArgs,
    mode: WriteMode,
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

    let conn = db::open_default()?;

    // Existence guard — create must not clobber, update must not conjure.
    let exists = db::secrets::get(&conn, &args.name)?.is_some();
    existence_guard(mode, exists, &args.name)?;

    let ref_path_for_storage = match args.backend.as_str() {
        "inline" => String::new(),
        _ => args.ref_path.clone().unwrap(),
    };
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

/// [MUTATES STATE] Create a new secret. Fails if a secret with this name already
/// exists — use `secrets.update` or `secrets.upsert` to change an existing one. For
/// 'inline' backend, `value` is required; for external backends, `ref_path` is
/// required (e.g. 'op://Vault/Item/field'). Write the secret on a remote system
/// with the top-level `--peer <h>` flag.
#[orca_tool(domain = "secrets", verb = "create")]
async fn secret_create(
    args: SecretWriteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    write_secret(args, WriteMode::Create).await
}

/// [MUTATES STATE] Update an existing secret's value/backend/metadata. Fails if
/// no secret with this name exists — use `secrets.create` to add it or
/// `secrets.upsert` to create-or-replace. For 'inline' backend, `value` is
/// required; for external backends, `ref_path` is required.
#[orca_tool(domain = "secrets", verb = "update")]
async fn secret_update(
    args: SecretWriteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    write_secret(args, WriteMode::Update).await
}

/// [MUTATES STATE] Idempotent upsert — create the secret if absent, replace it if
/// present. The automation-friendly write used for credential rotation. For
/// 'inline' backend, `value` is required; for external backends, `ref_path` is
/// required (e.g. 'op://Vault/Item/field').
#[orca_tool(domain = "secrets", verb = "upsert")]
async fn secret_upsert(
    args: SecretWriteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    write_secret(args, WriteMode::Upsert).await
}

/// [MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends
/// only the orca registration is removed (the upstream vault is untouched).
#[orca_tool(domain = "secrets", verb = "delete")]
async fn secret_delete(
    args: SecretDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretDeleteReport> {
    let conn = db::open_default()?;
    let removed = db::secrets::delete(&conn, &args.name)?;
    Ok(SecretDeleteReport {
        name: args.name,
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_refuses_to_clobber_existing() {
        assert!(existence_guard(WriteMode::Create, true, "s").is_err());
        assert!(existence_guard(WriteMode::Create, false, "s").is_ok());
    }

    #[test]
    fn update_refuses_to_conjure_missing() {
        assert!(existence_guard(WriteMode::Update, false, "s").is_err());
        assert!(existence_guard(WriteMode::Update, true, "s").is_ok());
    }

    #[test]
    fn upsert_accepts_either_state() {
        assert!(existence_guard(WriteMode::Upsert, true, "s").is_ok());
        assert!(existence_guard(WriteMode::Upsert, false, "s").is_ok());
    }

    #[test]
    fn guard_errors_name_the_alternative_verbs() {
        let e = existence_guard(WriteMode::Create, true, "tok")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("secrets.update") && e.contains("secrets.upsert"),
            "{e}"
        );
        let e = existence_guard(WriteMode::Update, false, "tok")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("secrets.create") && e.contains("secrets.upsert"),
            "{e}"
        );
    }
}
