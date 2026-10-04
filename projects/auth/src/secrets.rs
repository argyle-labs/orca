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

// ── Backends ────────────────────────────────────────────────────────────────

/// Fetch a value by name. Returns `(backend_kind, value)`. Used by tools and by
/// internal callers (e.g. lifecycle::resolve_github_token) that need a raw secret
/// without going through `#[orca_tool]` dispatch. `inline` secrets read the value
/// from the encrypted DB; any other backend is resolved by the matching
/// registered `secrets_backend` plugin.
pub async fn get_secret(name: &str) -> anyhow::Result<(String, String)> {
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
    Ok((row.backend, value))
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List configured secrets (names + backends + metadata). Never returns values.
#[orca_tool(domain = "secrets", verb = "list")]
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
#[orca_tool(domain = "secrets", verb = "upsert", cli = manual)]
async fn secret_upsert(
    args: SecretWriteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SecretMutationReport> {
    write_secret(args).await
}

/// Consume `--value-stdin` into `args.value` by reading `src`. Split out from
/// the CliOp so the conflict / newline / empty rules are unit-testable without
/// a real stdin.
fn resolve_value_from_stdin<R: std::io::Read>(
    args: &mut SecretWriteArgs,
    src: &mut R,
) -> anyhow::Result<()> {
    if !args.value_stdin {
        return Ok(());
    }
    if args.value.is_some() {
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
    args.value = Some(value.to_string());
    // Cleared so the flag can never be serialized onto the wire.
    args.value_stdin = false;
    Ok(())
}

/// Manual CLI block for `orca secrets upsert`. The generated `register_op!`
/// parses args and dispatches straight to the daemon, but `--value-stdin` has
/// to be consumed in the *calling* process — only the CLI has a stdin. Same
/// reasoning as `orca auth login`'s hand-rolled block: a purely client-side
/// affordance the daemon must not know about. The dispatch below mirrors
/// `register_op!`'s execute-gated path verbatim so dry-run-by-default still
/// holds for this verb.
const _: () = {
    use __cp::contract::{OrcaToolDef, plan::EXECUTE_FIELD};
    use __cp::dispatch::cli::{CliBuildFn, CliOp, CliRunFn};
    use ::plugin_toolkit as __cp;

    fn build() -> __cp::clap::Command {
        let cmd = __cp::clap::Command::new("upsert")
            .about(<SecretUpsert as OrcaToolDef>::DESCRIPTION)
            .arg(
                __cp::clap::Arg::new(EXECUTE_FIELD)
                    .long(EXECUTE_FIELD)
                    .action(__cp::clap::ArgAction::SetTrue)
                    .help(
                        "Apply the change. Omitted, this reports what WOULD change and \
                         changes nothing.",
                    ),
            );
        <<SecretUpsert as OrcaToolDef>::Args as __cp::clap::Args>::augment_args(cmd)
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
                <<SecretUpsert as OrcaToolDef>::Args as __cp::clap::FromArgMatches>::from_arg_matches(&m)
                    .map_err(|e| __cp::anyhow::anyhow!("{e}"))?;
            resolve_value_from_stdin(&mut args, &mut ::std::io::stdin().lock())?;

            let execute = m.get_flag(EXECUTE_FIELD);
            let value = __cp::dispatch::cli::exec_gated::<SecretUpsert>(
                args,
                execute,
                peer.as_deref(),
                &ctx,
            )
            .await?;
            if !execute {
                // The plan IS the answer here — print it as-is.
                println!(
                    "{}",
                    __cp::serde_json::to_string_pretty(&value)
                        .unwrap_or_else(|_| value.to_string())
                );
                return Ok(());
            }
            let out: <SecretUpsert as OrcaToolDef>::Output = __cp::serde_json::from_value(value)
                .map_err(|e| {
                    __cp::anyhow::anyhow!(
                        "decode {} output: {e}",
                        <SecretUpsert as OrcaToolDef>::NAME
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
            verb: "upsert",
            summary: <SecretUpsert as OrcaToolDef>::DESCRIPTION,
            build: build as CliBuildFn,
            run: run as CliRunFn,
        }
    }
};

/// [MUTATES STATE] Remove a secret. The inline value is zeroed; for external backends
/// only the orca registration is removed (the upstream vault is untouched).
#[orca_tool(domain = "secrets", verb = "delete")]
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
            ref_path: None,
            description: None,
        };
        assert!(write_secret(args).await.is_err());
    }

    #[tokio::test]
    async fn external_backend_upserts_and_reports_missing_registry_on_read() {
        let dir = std::env::temp_dir().join(format!("orca-secrets-test-{}", std::process::id()));
        let db_path = dir.join("orca.db");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let prev = std::env::var("ORCA_DB_PATH").ok();
        // SAFETY: single-threaded test; env is restored before returning.
        unsafe { std::env::set_var("ORCA_DB_PATH", &db_path) };

        // A secret pointing at an external backend needs no local plugin to write.
        let report = write_secret(SecretWriteArgs {
            name: "gh".into(),
            backend: "onepassword".into(),
            value: None,
            value_stdin: false,
            ref_path: Some("op://Personal/orca-gh/token".into()),
            description: None,
        })
        .await
        .expect("external-backend upsert succeeds");
        assert_eq!(report.backend, "onepassword");

        // With no `secrets_backend` plugin registered, resolution fails cleanly.
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
}
