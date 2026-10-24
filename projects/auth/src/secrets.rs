//! Secrets domain — named secrets with pluggable backends.
//!
//! Surface: `secrets.list`, `secrets.detail`, `secrets.upsert`,
//! `secrets.delete`, all admin-only. `upsert` is the idempotent
//! create-or-replace (HTTP PUT semantics) used for rotation and automation.
//! The only backend in v1 is `inline` (value stored in the
//! SQLCipher-encrypted orca.db). v2 plan adds 1Password / Bitwarden / OS
//! keychain backends as separate integration crates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use anyhow::{anyhow, bail};
use derive::orca_tool;

// ── Shared types ────────────────────────────────────────────────────────────

#[derive::snake_aliases]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SecretEntry {
    pub name: String,
    /// Backend kind. `"inline"` stores the value in the encrypted DB; any other
    /// kind (e.g. `"onepassword"`, `"bitwarden"`, `"vault"`) is resolved at read
    /// time by the matching registered `secrets_backend` plugin.
    pub backend: String,
    /// Backend-specific reference (e.g. `op://Personal/orca-gh/token`). Empty for inline.
    pub ref_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub updated_at: String,
}

// ── secret.list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SecretListArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive::snake_aliases]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretListReport {
    pub secrets: Vec<SecretEntry>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
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

#[derive::snake_aliases]
#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
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
    /// CLI only: read the value from stdin instead of `--value`, so the secret
    /// never lands in `argv` where any local process can read it via `ps`.
    /// Consumed into `value` in the calling process (see the manual CliOp at
    /// the bottom of this file) and never serialized, so REST/MCP — which have
    /// no stdin — can't be asked to honour it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[arg(long)]
    pub value_stdin: bool,
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

#[derive::snake_aliases]
#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretPutArgs {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub value: Option<String>,
    /// CLI only: read the value from stdin instead of `--value`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[arg(long)]
    pub value_stdin: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SecretRestoreArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretValueChangeReport {
    pub name: String,
    pub backend: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SecretBackendListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendEntry {
    pub backend: String,
    pub capabilities: contract::secrets_backend::BackendCapabilities,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretBackendListReport {
    pub backends: Vec<SecretBackendEntry>,
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SecretGrantArgs {
    /// Plugin id that may read the secret.
    pub holder: String,
    /// Name of an existing secret outside the holder's namespace.
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGrantReport {
    pub holder: String,
    pub name: String,
    pub changed: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct SecretGrantListArgs {
    #[arg(long)]
    pub holder: Option<String>,
}

#[derive::snake_aliases]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretGrantEntry {
    pub holder: String,
    pub secret_name: String,
    pub granted_by: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SecretGrantListReport {
    pub grants: Vec<SecretGrantEntry>,
}

// ── Backends ────────────────────────────────────────────────────────────────

/// Fetch a value by name. Returns `(backend_kind, value)`. Used by tools and by
/// internal callers (e.g. lifecycle::resolve_github_token) that need a raw secret
/// without going through `#[orca_tool]` dispatch. `inline` secrets read the value
/// from the encrypted DB; any other backend is resolved by the matching
/// registered `secrets_backend` plugin.
pub async fn get_secret(name: &str) -> anyhow::Result<(String, utils::secret::Secret<String>)> {
    // All DB access goes through the pool seam. (Production never sets a
    // task-/thread-local DB override, so the pool resolves the canonical
    // encrypted db; test isolation is handled by scoped overrides.)
    let row = db::pool::Db::process()
        .read(|c| secrets::get(c, name))?
        .ok_or_else(|| anyhow!("no secret named '{name}'"))?;
    let value = match row.backend.as_str() {
        "inline" => db::pool::Db::process()
            .read(|c| secrets::read_inline_value(c, &row.name))?
            .ok_or_else(|| anyhow!("inline secret '{}' has no stored value", row.name))?,
        _ => contract::secrets_backend::resolve(&row.backend, &row.ref_path).await?,
    };
    Ok((row.backend, utils::secret::Secret::new(value)))
}

fn secret_record(name: &str) -> anyhow::Result<secrets::SecretRecord> {
    db::pool::Db::process()
        .read(|c| secrets::get(c, name))?
        .ok_or_else(|| anyhow!("no secret named '{name}'"))
}

pub async fn backend_capabilities(
    backend: &str,
) -> anyhow::Result<contract::secrets_backend::BackendCapabilities> {
    if backend == "inline" {
        return Ok(contract::secrets_backend::BackendCapabilities::writable());
    }
    contract::secrets_backend::capabilities(backend).await
}

pub async fn put_secret(name: &str, value: &str) -> anyhow::Result<String> {
    if value.is_empty() {
        bail!("refusing to write an empty value to secret '{name}'");
    }
    let row = secret_record(name)?;
    match row.backend.as_str() {
        "inline" => db::pool::Db::process().write(|c| secrets::put_inline_value(c, name, value))?,
        _ => contract::secrets_backend::put(&row.backend, &row.ref_path, value).await?,
    }
    Ok(row.backend)
}

pub async fn restore_secret(name: &str) -> anyhow::Result<String> {
    let row = secret_record(name)?;
    match row.backend.as_str() {
        "inline" => db::pool::Db::process().write(|c| secrets::restore_inline_value(c, name))?,
        _ => contract::secrets_backend::restore_previous(&row.backend, &row.ref_path).await?,
    }
    Ok(row.backend)
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List configured secrets (names + backends + metadata). Never returns values.
// Admin despite being read-shaped: `ref_path` discloses external vault layout.
#[orca_tool(domain = "secrets", verb = "list", role = "admin")]
async fn secret_list(
    args: SecretListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretListReport> {
    let mut rows = db::pool::Db::process().read(secrets::list)?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let secrets: Vec<SecretEntry> = rows
        .into_iter()
        .map(|r| SecretEntry {
            name: r.name,
            backend: r.backend,
            ref_path: r.ref_path,
            description: r.description,
            updated_at: r.updated_at,
        })
        .collect();
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(secrets, &params);
    Ok(SecretListReport {
        secrets: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

/// [SENSITIVE] Fetch a secret value by name. Resolves via the configured backend.
// Admin despite being read-shaped: returns the plaintext value.
#[orca_tool(domain = "secrets", verb = "detail", role = "admin")]
async fn secret_detail(
    args: SecretGetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretGetReport> {
    let (backend, value) = get_secret(&args.name).await?;
    Ok(SecretGetReport {
        name: args.name,
        backend,
        value: value.expose_secret().clone(),
    })
}

/// The `secrets.upsert` write path. Validates the backend + required fields,
/// then upserts the metadata row and (for `inline`) the encrypted value.
/// Idempotent create-or-replace (HTTP PUT semantics).
async fn write_secret(args: SecretWriteArgs) -> anyhow::Result<SecretMutationReport> {
    // Field-based, backend-agnostic validation: `inline` stores a value locally;
    // every other backend is resolved by a registered `secrets_backend` plugin
    // from its `ref_path`, so we don't gate on a known-backends list here.
    // Reaching the shared write path with this still set means a REST/MCP
    // caller sent it; those surfaces have no stdin, so reject rather than
    // silently ignore (same stance as the retired `--scope` field).
    if args.value_stdin {
        bail!("`value_stdin` is a CLI-only flag; REST/MCP callers must send `value`");
    }
    match args.backend.as_str() {
        "inline" => {
            if args.value.is_none() {
                bail!("`value` is required for backend=inline");
            }
        }
        _ => {
            // Presence only. Core deliberately does NOT parse the reference:
            // `op://`/`bw://` syntax is vendor knowledge, and knowing it here
            // is what put a 1Password implementation in core in the first
            // place. The owning backend plugin validates its own refs.
            secrets::ensure_backend_registered(&args.backend)?;
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
    let created = db::pool::Db::process().write(|conn| {
        let created = secrets::upsert(
            conn,
            &args.name,
            &args.backend,
            &ref_path_for_storage,
            args.description.as_deref(),
        )?;
        if args.backend == "inline" {
            secrets::write_inline_value(conn, &args.name, args.value.as_deref().unwrap_or(""))?;
        }
        Ok(created)
    })?;
    Ok(SecretMutationReport {
        name: args.name,
        backend: args.backend,
        created,
    })
}

/// [MUTATES STATE] Idempotent upsert — create the secret if absent, replace it if
/// present. The single canonical write, used for credential rotation. For
/// 'inline' backend, `value` is required; for external backends, `ref_path` is
/// required (e.g. 'op://Vault/Item/field'). Write the secret on a remote system
/// with the top-level `--peer <h>` flag.
#[orca_tool(domain = "secrets", verb = "upsert", cli = manual, data_mutation = false)]
async fn secret_upsert(
    args: SecretWriteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    write_secret(args).await
}

/// [SENSITIVE] [MUTATES STATE] Write a new value for an existing secret through
/// its backend, keeping the replaced value restorable with `secrets restore`.
/// Fails cleanly when the backend reports write as unavailable.
#[orca_tool(domain = "secrets", verb = "put", cli = manual, role = "admin", data_mutation = false)]
async fn secret_put(
    args: SecretPutArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretValueChangeReport> {
    if args.value_stdin {
        bail!("`value_stdin` is a CLI-only flag; REST/MCP callers must send `value`");
    }
    let value = args.value.ok_or_else(|| anyhow!("`value` is required"))?;
    let backend = put_secret(&args.name, &value).await?;
    Ok(SecretValueChangeReport {
        name: args.name,
        backend,
    })
}

/// [SENSITIVE] [MUTATES STATE] Restore the value a secret held before its last
/// `secrets put`.
#[orca_tool(
    domain = "secrets",
    verb = "restore",
    role = "admin",
    data_mutation = false
)]
async fn secret_restore(
    args: SecretRestoreArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretValueChangeReport> {
    let backend = restore_secret(&args.name).await?;
    Ok(SecretValueChangeReport {
        name: args.name,
        backend,
    })
}

/// List secrets backends on this host with their resolve/put/restore capability.
#[orca_tool(domain = "secrets.backends", verb = "list", role = "admin")]
async fn secret_backend_list(
    _args: SecretBackendListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretBackendListReport> {
    let mut names: Vec<String> = contract::secrets_backend::providers()
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    names.sort();
    names.insert(0, "inline".into());
    let mut backends = Vec::with_capacity(names.len());
    for backend in names {
        let capabilities = backend_capabilities(&backend).await?;
        backends.push(SecretBackendEntry {
            backend,
            capabilities,
        });
    }
    Ok(SecretBackendListReport { backends })
}

trait StdinValue {
    fn value_slots(&mut self) -> (&mut Option<String>, &mut bool);
}

impl StdinValue for SecretWriteArgs {
    fn value_slots(&mut self) -> (&mut Option<String>, &mut bool) {
        (&mut self.value, &mut self.value_stdin)
    }
}

impl StdinValue for SecretPutArgs {
    fn value_slots(&mut self) -> (&mut Option<String>, &mut bool) {
        (&mut self.value, &mut self.value_stdin)
    }
}

/// Consume `--value-stdin` into `args.value` by reading `src`. Split out from
/// the CliOp so the conflict / newline / empty rules are unit-testable without
/// a real stdin.
fn resolve_value_from_stdin<A: StdinValue, R: std::io::Read>(
    args: &mut A,
    src: &mut R,
) -> anyhow::Result<()> {
    let (value_slot, value_stdin) = args.value_slots();
    if !*value_stdin {
        return Ok(());
    }
    if value_slot.is_some() {
        bail!("pass one of --value or --value-stdin, not both");
    }
    let mut buf = String::new();
    src.read_to_string(&mut buf)
        .map_err(|e| anyhow!("read secret value from stdin: {e}"))?;
    // Shells and `printf` append a line ending; a stored token with a trailing
    // "\n" silently breaks every consumer. Strip only that line ending —
    // leading and interior whitespace can be part of the secret.
    let value = buf.strip_suffix('\n').unwrap_or(&buf);
    let value = value.strip_suffix('\r').unwrap_or(value);
    if value.is_empty() {
        bail!("--value-stdin got empty stdin; pipe the secret value in");
    }
    *value_slot = Some(value.to_string());
    // Cleared so the flag can never be serialized onto the wire.
    *value_stdin = false;
    Ok(())
}

/// Manual CLI block for `orca secrets upsert` and `orca secrets put`. The generated `register_op!`
/// parses args and dispatches straight to the daemon, but `--value-stdin` has
/// to be consumed in the *calling* process — only the CLI has a stdin. Same
/// reasoning as `orca auth login`'s hand-rolled block: a purely client-side
/// affordance the daemon must not know about. The dispatch below mirrors
/// `register_op!`'s execute-gated path verbatim so `--dry-run` holds for this
/// verb.
macro_rules! stdin_value_cli {
    ($tool:ident, $verb:literal) => {
const _: () = {
    use __cp::contract::{
        OrcaToolDef,
        plan::{DRY_RUN_FIELD, EXECUTE_FIELD},
    };
    use __cp::dispatch::cli::{CliBuildFn, CliOp, CliRunFn};
    use ::plugin_toolkit as __cp;

    fn build() -> __cp::clap::Command {
        let cmd = __cp::clap::Command::new($verb)
            .about(<$tool as OrcaToolDef>::DESCRIPTION)
            .arg(
                __cp::clap::Arg::new(DRY_RUN_FIELD)
                    .long(__cp::dispatch::cli::DRY_RUN_FLAG)
                    .action(__cp::clap::ArgAction::SetTrue)
                    .help("Report what WOULD change and change nothing. Omitted, the change is applied."),
            )
            .arg(
                __cp::clap::Arg::new(EXECUTE_FIELD)
                    .long(EXECUTE_FIELD)
                    .action(__cp::clap::ArgAction::SetTrue)
                    .hide(true),
            );
        <<$tool as OrcaToolDef>::Args as __cp::clap::Args>::augment_args(cmd)
    }

    fn run(
        m: &__cp::clap::ArgMatches,
        ctx: ::std::sync::Arc<__cp::contract::ToolCtx>,
    ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = __cp::anyhow::Result<()>> + Send>>
    {
        let m = m.clone();
        Box::pin(async move {
            let peer = ctx.peer().map(|s| s.to_string());
            let mut args =
                <<$tool as OrcaToolDef>::Args as __cp::clap::FromArgMatches>::from_arg_matches(&m)
                    .map_err(|e| __cp::anyhow::anyhow!("{e}"))?;
            resolve_value_from_stdin(&mut args, &mut ::std::io::stdin().lock())?;

            let dry_run = m.get_flag(DRY_RUN_FIELD);
            let value = __cp::dispatch::cli::exec_gated::<$tool>(
                args,
                dry_run,
                peer.as_deref(),
                &ctx,
            )
            .await?;
            if dry_run {
                // The plan IS the answer here — print it as-is.
                println!(
                    "{}",
                    __cp::serde_json::to_string_pretty(&value)
                        .unwrap_or_else(|_| value.to_string())
                );
                return Ok(());
            }
            let out: <$tool as OrcaToolDef>::Output = __cp::serde_json::from_value(value)
                .map_err(|e| {
                    __cp::anyhow::anyhow!(
                        "decode {} output: {e}",
                        <$tool as OrcaToolDef>::NAME
                    )
                })?;
            let s = __cp::serde_json::to_string_pretty(&out)
                .unwrap_or_else(|e| format!("<unserializable output: {e}>"));
            println!("{s}");
            Ok(())
        })
    }

    __cp::inventory::submit! {
        CliOp {
            domain: "secrets",
            verb: $verb,
            summary: <$tool as OrcaToolDef>::DESCRIPTION,
            build: build as CliBuildFn,
            run: run as CliRunFn,
        }
    }
};
    };
}

stdin_value_cli!(SecretUpsert, "upsert");
stdin_value_cli!(SecretPut, "put");

/// [MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends
/// only the orca registration is removed (the upstream vault is untouched).
#[orca_tool(domain = "secrets", verb = "delete", data_mutation = false)]
async fn secret_delete(
    args: SecretDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretDeleteReport> {
    let removed = db::pool::Db::process().write(|conn| secrets::delete(conn, &args.name))?;
    Ok(SecretDeleteReport {
        name: args.name,
        removed,
    })
}

fn caller_name(ctx: &contract::ToolCtx) -> anyhow::Result<String> {
    ctx.verified_caller()
        .cloned()
        .or_else(|| ctx.caller())
        .map(|c| c.username)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("no caller identity"))
}

/// [MUTATES STATE] Grant a plugin read access to an existing secret outside its namespace.
#[orca_tool(
    domain = "secrets.grant",
    verb = "create",
    role = "admin",
    data_mutation = false
)]
async fn secret_grant_create(
    args: SecretGrantArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretGrantReport> {
    let by = caller_name(ctx)?;
    let changed = db::pool::Db::process()
        .write(|conn| secrets::grant_checked(conn, &args.holder, &args.name, &by))?;
    tracing::info!(target: "audit", caller = %by, holder = %args.holder, secret = %args.name, changed, "secret grant");
    Ok(SecretGrantReport {
        holder: args.holder,
        name: args.name,
        changed,
    })
}

/// [MUTATES STATE] Revoke a plugin's grant on a secret.
#[orca_tool(
    domain = "secrets.grant",
    verb = "delete",
    role = "admin",
    data_mutation = false
)]
async fn secret_grant_delete(
    args: SecretGrantArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretGrantReport> {
    let by = caller_name(ctx)?;
    let changed =
        db::pool::Db::process().write(|conn| secrets::revoke(conn, &args.holder, &args.name))?;
    tracing::info!(target: "audit", caller = %by, holder = %args.holder, secret = %args.name, changed, "secret revoke");
    Ok(SecretGrantReport {
        holder: args.holder,
        name: args.name,
        changed,
    })
}

/// List secret grants, optionally for one holder. Never returns values.
#[orca_tool(domain = "secrets.grant", verb = "list", role = "admin")]
async fn secret_grant_list(
    args: SecretGrantListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretGrantListReport> {
    let grants = db::pool::Db::process()
        .read(secrets::list_grants)?
        .into_iter()
        .filter(|g| args.holder.as_ref().is_none_or(|h| *h == g.holder))
        .map(|g| SecretGrantEntry {
            holder: g.holder,
            secret_name: g.secret_name,
            granted_by: g.granted_by,
            created_at: g.created_at,
        })
        .collect();
    Ok(SecretGrantListReport { grants })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inline_args(value: Option<&str>, value_stdin: bool) -> SecretWriteArgs {
        SecretWriteArgs {
            name: "s".into(),
            backend: "inline".into(),
            value: value.map(str::to_string),
            value_stdin,
            ref_path: None,
            description: None,
        }
    }

    #[test]
    fn every_secrets_verb_is_admin_only() {
        use contract::OrcaToolDef;
        assert_eq!(<SecretList as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretDetail as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretUpsert as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretDelete as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretGrantCreate as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretGrantDelete as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretGrantList as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretPut as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretRestore as OrcaToolDef>::REQUIRED_ROLE, "admin");
        assert_eq!(<SecretBackendList as OrcaToolDef>::REQUIRED_ROLE, "admin");
        const { assert!(<SecretPut as OrcaToolDef>::EXECUTE_GATED) };
        const { assert!(<SecretRestore as OrcaToolDef>::EXECUTE_GATED) };
        const { assert!(!<SecretPut as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretRestore as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretGrantCreate as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretGrantDelete as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretGrantList as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(<SecretGrantCreate as OrcaToolDef>::EXECUTE_GATED) };
        const { assert!(<SecretGrantDelete as OrcaToolDef>::EXECUTE_GATED) };
        // DATA_MUTATION would let a non-admin `can_mutate` caller through.
        const { assert!(!<SecretList as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretDetail as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretUpsert as OrcaToolDef>::DATA_MUTATION) };
        const { assert!(!<SecretDelete as OrcaToolDef>::DATA_MUTATION) };
    }

    #[test]
    fn value_stdin_strips_only_the_trailing_line_ending() {
        for (input, want) in [
            ("tok\n", "tok"),
            ("tok\r\n", "tok"),
            ("tok", "tok"),
            // Interior newlines and surrounding spaces survive.
            ("  a b\nc \n", "  a b\nc "),
        ] {
            let mut args = inline_args(None, true);
            resolve_value_from_stdin(&mut args, &mut input.as_bytes()).expect("resolve");
            assert_eq!(args.value.as_deref(), Some(want), "input {input:?}");
            assert!(!args.value_stdin, "flag must be cleared before dispatch");
        }
    }

    #[test]
    fn value_and_value_stdin_together_is_an_error() {
        let mut args = inline_args(Some("tok"), true);
        let err = resolve_value_from_stdin(&mut args, &mut "other\n".as_bytes())
            .expect_err("both flags rejected");
        assert!(
            err.to_string()
                .contains("pass one of --value or --value-stdin"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_stdin_is_an_error_not_an_empty_secret() {
        for input in ["", "\n", "\r\n"] {
            let mut args = inline_args(None, true);
            assert!(
                resolve_value_from_stdin(&mut args, &mut input.as_bytes()).is_err(),
                "input {input:?} must not store an empty secret"
            );
        }
    }

    #[test]
    fn value_flag_is_untouched_without_value_stdin() {
        let mut args = inline_args(Some("tok"), false);
        resolve_value_from_stdin(&mut args, &mut "ignored\n".as_bytes()).expect("resolve");
        assert_eq!(args.value.as_deref(), Some("tok"));
    }

    #[test]
    fn value_stdin_parses_as_a_cli_flag() {
        use clap::{Args, FromArgMatches};
        let cmd = SecretWriteArgs::augment_args(clap::Command::new("upsert"));
        let m = cmd.get_matches_from(["upsert", "s", "--value-stdin"]);
        let args = SecretWriteArgs::from_arg_matches(&m).expect("parse");
        assert!(args.value_stdin);
        assert!(args.value.is_none());
    }

    #[tokio::test]
    async fn write_rejects_value_stdin_from_a_stdinless_surface() {
        let Err(err) = write_secret(inline_args(None, true)).await else {
            panic!("value_stdin must be rejected off the CLI");
        };
        assert!(
            err.to_string().contains("CLI-only"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_backend() {
        let args = SecretWriteArgs {
            name: "s".into(),
            backend: "nope".into(),
            value: None,
            value_stdin: false,
            ref_path: Some("x://y".into()),
            description: None,
        };
        let Err(err) = write_secret(args).await else {
            panic!("unregistered backend must be rejected");
        };
        let err = err.to_string();
        assert!(err.contains("'nope' is not registered"), "{err}");
    }

    #[tokio::test]
    async fn external_backend_upserts_and_reports_missing_registry_on_read() {
        let dir = std::env::temp_dir().join(format!("orca-secrets-test-{}", std::process::id()));
        let db_path = dir.join("orca.db");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let prev = std::env::var("ORCA_DB_PATH").ok();
        // SAFETY: single-threaded test; env is restored before returning.
        unsafe { std::env::set_var("ORCA_DB_PATH", &db_path) };

        struct Upsertable;
        #[async_trait::async_trait]
        impl contract::secrets_backend::SecretsBackend for Upsertable {
            fn name(&self) -> &str {
                "upsert-test-backend"
            }
            async fn resolve(&self, _ref_path: &str) -> anyhow::Result<String> {
                anyhow::bail!("unused")
            }
        }
        contract::secrets_backend::register_provider(std::sync::Arc::new(Upsertable));
        let report = write_secret(SecretWriteArgs {
            name: "gh".into(),
            backend: "upsert-test-backend".into(),
            value: None,
            value_stdin: false,
            ref_path: Some("op://Personal/orca-gh/token".into()),
            description: None,
        })
        .await
        .expect("external-backend upsert succeeds");
        assert_eq!(report.backend, "upsert-test-backend");
        assert!(contract::secrets_backend::deregister_provider(
            "upsert-test-backend"
        ));

        let err = get_secret("gh").await.expect_err("no backend registered");
        assert!(
            err.to_string().contains("no secrets backend"),
            "unexpected error: {err}"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("ORCA_DB_PATH", v) },
            None => unsafe { std::env::remove_var("ORCA_DB_PATH") },
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    struct WriteBackend {
        caps: contract::secrets_backend::BackendCapabilities,
        puts: std::sync::Mutex<Vec<(String, String)>>,
        restores: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl contract::secrets_backend::SecretsBackend for WriteBackend {
        fn name(&self) -> &str {
            if self.caps.put.is_available() {
                "auth-test-writable"
            } else {
                "auth-test-readonly"
            }
        }
        async fn resolve(&self, _ref_path: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn capabilities(&self) -> contract::secrets_backend::BackendCapabilities {
            self.caps.clone()
        }
        async fn put(&self, ref_path: &str, value: &str) -> anyhow::Result<()> {
            self.puts
                .lock()
                .unwrap()
                .push((ref_path.to_string(), value.to_string()));
            Ok(())
        }
        async fn restore_previous(&self, ref_path: &str) -> anyhow::Result<()> {
            self.restores.lock().unwrap().push(ref_path.to_string());
            Ok(())
        }
    }

    fn write_backend(
        caps: contract::secrets_backend::BackendCapabilities,
    ) -> std::sync::Arc<WriteBackend> {
        std::sync::Arc::new(WriteBackend {
            caps,
            puts: Default::default(),
            restores: Default::default(),
        })
    }

    #[tokio::test]
    async fn put_and_restore_route_through_the_secret_backend() {
        use contract::secrets_backend::BackendCapabilities;
        let dir = tempfile::tempdir().expect("tempdir");
        db::with_db_path(dir.path().join("orca.db"), async {
            write_secret(inline_args(Some("one"), false))
                .await
                .expect("seed inline");
            assert_eq!(put_secret("s", "two").await.unwrap(), "inline");
            assert_eq!(get_secret("s").await.unwrap().1.expose_secret(), "two");
            restore_secret("s").await.unwrap();
            assert_eq!(get_secret("s").await.unwrap().1.expose_secret(), "one");
            assert!(put_secret("s", "").await.is_err());

            let writable = write_backend(BackendCapabilities::writable());
            contract::secrets_backend::register_provider(writable.clone());
            let readonly = write_backend(BackendCapabilities::write_unavailable(
                "service account is read-only",
            ));
            contract::secrets_backend::register_provider(readonly.clone());
            for (name, backend) in [("w", "auth-test-writable"), ("r", "auth-test-readonly")] {
                write_secret(SecretWriteArgs {
                    name: name.into(),
                    backend: backend.into(),
                    value: None,
                    value_stdin: false,
                    ref_path: Some(format!("op://v/{name}/password")),
                    description: None,
                })
                .await
                .expect("seed external");
            }
            put_secret("w", "new").await.unwrap();
            restore_secret("w").await.unwrap();
            assert_eq!(
                *writable.puts.lock().unwrap(),
                vec![("op://v/w/password".to_string(), "new".to_string())]
            );
            assert_eq!(
                *writable.restores.lock().unwrap(),
                vec!["op://v/w/password"]
            );

            let err = put_secret("r", "new").await.unwrap_err().to_string();
            assert!(err.contains("service account is read-only"), "{err}");
            assert!(readonly.puts.lock().unwrap().is_empty());
            let caps = backend_capabilities("auth-test-readonly").await.unwrap();
            assert!(!caps.put.is_available());
            assert!(
                backend_capabilities("inline")
                    .await
                    .unwrap()
                    .put
                    .is_available()
            );

            contract::secrets_backend::deregister_provider("auth-test-writable");
            contract::secrets_backend::deregister_provider("auth-test-readonly");
        })
        .await;
    }

    #[test]
    fn put_value_stdin_is_consumed_like_upsert() {
        let mut args = SecretPutArgs {
            name: "s".into(),
            value: None,
            value_stdin: true,
        };
        resolve_value_from_stdin(&mut args, &mut "tok\n".as_bytes()).expect("resolve");
        assert_eq!(args.value.as_deref(), Some("tok"));
        assert!(!args.value_stdin);
    }
}
