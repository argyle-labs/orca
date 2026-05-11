//! Unified CLI surface for `OrcaOp`-flavoured tools.
//!
//! Each migrated op contributes one `CliOp` to a linker-time inventory via
//! the [`register_op!`] macro. The orca binary walks the inventory once at
//! startup to build a clap `Command` tree (`orca <domain> <verb> [args]`)
//! and dispatches matched args back through the tool's own `OrcaTool::run`.
//!
//! Why this exists: before this module, every tool needed a hand-written
//! `commands/<domain>_cmd.rs` shim duplicating arg parsing + dispatch that
//! already lives on the tool. With `register_op!`, that file goes away —
//! Args/Output flow end-to-end across MCP/REST/WASM/CLI from one source.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use clap::{ArgMatches, Command};
use orca_utils::tool::ToolCtx;

/// Erased CLI dispatch closure: parses matches into the op's Args struct,
/// invokes `OrcaTool::run`, formats Output to stdout.
pub type CliRunFn =
    fn(&ArgMatches, Arc<ToolCtx>) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// Erased clap-subcommand builder for a single op.
pub type CliBuildFn = fn() -> Command;

/// One entry per `register_op!`. The orca binary collects these via the
/// `inventory` crate (linker-time) and groups by `domain` to assemble the
/// `orca <domain>` subcommand tree.
pub struct CliOp {
    pub domain: &'static str,
    pub verb: &'static str,
    pub summary: &'static str,
    pub build: CliBuildFn,
    pub run: CliRunFn,
}

inventory::collect!(CliOp);

/// Iterate over every registered CLI op. Stable order is **not** guaranteed
/// (`inventory` is linker-order); callers that need stable order should sort
/// by `(domain, verb)`.
pub fn ops() -> impl Iterator<Item = &'static CliOp> {
    inventory::iter::<CliOp>()
}

/// Build the top-level `orca` clap command from every registered op.
/// Domains become subcommands; verbs become sub-subcommands.
pub fn build_root(mut root: Command) -> Command {
    use std::collections::BTreeMap;

    let mut by_domain: BTreeMap<&'static str, Vec<&'static CliOp>> = BTreeMap::new();
    for op in ops() {
        by_domain.entry(op.domain).or_default().push(op);
    }

    for (domain, mut ops) in by_domain {
        ops.sort_by_key(|o| o.verb);
        let mut dom = Command::new(domain)
            .about(format!("Manage {domain}"))
            .subcommand_required(true)
            .arg_required_else_help(true);
        for op in ops {
            dom = dom.subcommand((op.build)());
        }
        root = root.subcommand(dom);
    }
    root
}

/// Try to dispatch one parsed clap match through the inventory.
/// Returns `Some(result)` if the (domain, verb) pair was found and ran;
/// `None` if no match — caller should fall through to legacy dispatch.
pub async fn try_dispatch(matches: &ArgMatches, ctx: Arc<ToolCtx>) -> Option<Result<()>> {
    let (domain, dom_matches) = matches.subcommand()?;
    let (verb, op_matches) = dom_matches.subcommand()?;
    let op = ops().find(|o| o.domain == domain && o.verb == verb)?;
    Some((op.run)(op_matches, ctx).await)
}

/// Register one op with the unified CLI surface.
///
/// ```ignore
/// register_op! {
///     tool: EngineList,
///     domain: "engine",
///     verb: "list",
///     summary: "List registered LLM backends",
///     render: |out| {
///         for p in out.0 { println!("{} {}", p.name, p.url); }
///     }
/// }
/// ```
///
/// Requires `Tool::Args: clap::Args` so the macro can auto-derive the
/// subcommand's flags from the same struct that MCP/REST already serialize.
#[macro_export]
macro_rules! register_op {
    // Default form: pretty-print the output as JSON. Use this during the
    // mechanical sweep and override per-tool when human-friendly output
    // matters (e.g. `engine list` colored table).
    (
        tool: $tool:path,
        domain: $domain:expr,
        verb: $verb:expr,
        summary: $summary:expr $(,)?
    ) => {
        $crate::register_op! {
            tool: $tool,
            domain: $domain,
            verb: $verb,
            summary: $summary,
            render: |out| {
                let s = ::serde_json::to_string_pretty(&out)
                    .unwrap_or_else(|e| format!("<unserializable output: {e}>"));
                println!("{s}");
            }
        }
    };
    (
        tool: $tool:path,
        domain: $domain:expr,
        verb: $verb:expr,
        summary: $summary:expr,
        render: |$out:ident| $render:block $(,)?
    ) => {
        const _: () = {
            use $crate::cli::{CliOp, CliBuildFn, CliRunFn};
            use $crate::OrcaToolDef;
            use ::orca_utils::tool::OrcaTool;

            fn build() -> clap::Command {
                let cmd = clap::Command::new($verb).about($summary);
                <<$tool as OrcaToolDef>::Args as clap::Args>::augment_args(cmd)
            }

            fn run(
                m: &clap::ArgMatches,
                ctx: ::std::sync::Arc<::orca_utils::tool::ToolCtx>,
            ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = ::anyhow::Result<()>> + Send>> {
                let m = m.clone();
                Box::pin(async move {
                    let args = <<$tool as OrcaToolDef>::Args as clap::FromArgMatches>::from_arg_matches(&m)
                        .map_err(|e| ::anyhow::anyhow!("{e}"))?;
                    let $out = <$tool as OrcaTool>::run(args, &ctx).await?;
                    { $render }
                    Ok(())
                })
            }

            ::inventory::submit! {
                CliOp {
                    domain: $domain,
                    verb: $verb,
                    summary: $summary,
                    build: build as CliBuildFn,
                    run: run as CliRunFn,
                }
            }
        };
    };
}
