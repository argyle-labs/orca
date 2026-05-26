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

use crate::ToolCtx;

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
/// Domains become subcommands; verbs become sub-subcommands. A dotted domain
/// (`"pod.peer"`) nests further: `orca pod peer list` rather than the literal
/// `orca pod.peer list`. The dotted form remains the canonical tool NAME on
/// REST/MCP/WASM (`pod.peer.list`); only the CLI surface splits on the dots.
pub fn build_root(mut root: Command) -> Command {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        children: BTreeMap<&'static str, Node>,
        ops: Vec<&'static CliOp>,
    }

    let mut tree = Node::default();
    for op in ops() {
        let mut cur = &mut tree;
        for seg in op.domain.split('.') {
            cur = cur.children.entry(seg).or_default();
        }
        cur.ops.push(op);
    }

    fn materialize(name: &'static str, mut node: Node) -> Command {
        let mut cmd = Command::new(name)
            .about(format!("Manage {name}"))
            .subcommand_required(true)
            .arg_required_else_help(true);
        node.ops.sort_by_key(|o| o.verb);
        for op in node.ops {
            cmd = cmd.subcommand((op.build)());
        }
        for (child_name, child) in node.children {
            cmd = cmd.subcommand(materialize(child_name, child));
        }
        cmd
    }

    for (name, node) in tree.children {
        root = root.subcommand(materialize(name, node));
    }
    root
}

pub use crate::RemoteExec;

/// Run an OrcaTool on a paired peer with end-to-end typed Args/Output. The
/// JSON serialization happens internally — the call site, the trait API, and
/// the rendered output all stay typed. JsonAny / Value never appear.
///
/// Peer dispatch goes through whatever `RemoteExec` the host registered on
/// the `ToolCtx`. The remote allowlist (`REMOTE_OK = true` on the tool) is
/// enforced on the peer side; calls to non-remote-ok tools surface here as
/// an error.
pub async fn exec_remote<T: crate::OrcaToolDef>(
    peer: &str,
    args: T::Args,
    ctx: &ToolCtx,
) -> Result<T::Output> {
    // Wire-only serialization: Value lives entirely behind the
    // RemoteExec trait boundary, never on a public type.
    #[allow(clippy::disallowed_types)]
    let args_value =
        serde_json::to_value(&args).map_err(|e| anyhow::anyhow!("serialize args: {e}"))?;
    let svc = ctx.service::<Arc<dyn RemoteExec>>()?;
    let result = svc.exec(peer, T::NAME, args_value).await?;
    #[allow(clippy::disallowed_types)]
    let out: T::Output = serde_json::from_value(result)
        .map_err(|e| anyhow::anyhow!("decode {} output from peer {peer}: {e}", T::NAME))?;
    Ok(out)
}

/// Try to dispatch one parsed clap match through the inventory.
/// Returns `Some(result)` if the (domain, verb) pair was found and ran;
/// `None` if no match — caller should fall through to legacy dispatch.
pub async fn try_dispatch(matches: &ArgMatches, ctx: Arc<ToolCtx>) -> Option<Result<()>> {
    let (domain, verb, op_matches) = walk_to_verb(matches)?;
    let op = ops().find(|o| o.domain == domain.as_str() && o.verb == verb)?;
    Some((op.run)(op_matches, ctx).await)
}

/// Walk nested subcommands to the verb leaf. A node whose `.subcommand()` is
/// `Some` is treated as a domain segment (`pod` → `peer`); the first node
/// without a further subcommand is the verb. Returns `(domain, verb,
/// verb_matches)`, where `domain` is the dotted concat of the traversed
/// segments. `None` when no subcommand was selected.
fn walk_to_verb(matches: &ArgMatches) -> Option<(String, &str, &ArgMatches)> {
    let mut domain_parts: Vec<&str> = Vec::new();
    let mut cur = matches.subcommand()?;
    let (verb, op_matches) = loop {
        let (name, sub) = cur;
        if sub.subcommand().is_some() {
            domain_parts.push(name);
            cur = sub.subcommand()?;
        } else {
            break (name, sub);
        }
    };
    Some((domain_parts.join("."), verb, op_matches))
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
            use $crate::{OrcaTool, OrcaToolDef};

            fn build() -> clap::Command {
                let cmd = clap::Command::new($verb).about($summary);
                let cmd = <<$tool as OrcaToolDef>::Args as clap::Args>::augment_args(cmd);
                // Cross-cutting: every tool gains `--peer <PEER>`. When set,
                // we ship the typed Args to that peer over pod/exec and
                // deserialize the typed Output back. REMOTE_OK gate is
                // enforced on the peer side; remote_ok=false tools 401.
                cmd.arg(
                    clap::Arg::new("__peer")
                        .long("peer")
                        .value_name("PEER")
                        .help("Run on a paired peer (hostname like 'willow', peer_id, addr, or `local`) instead of this host; ambiguous hostnames are rejected")
                        .required(false),
                )
            }

            fn run(
                m: &clap::ArgMatches,
                ctx: ::std::sync::Arc<$crate::ToolCtx>,
            ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = ::anyhow::Result<()>> + Send>> {
                let m = m.clone();
                Box::pin(async move {
                    let peer = m.get_one::<String>("__peer").cloned();
                    let args = <<$tool as OrcaToolDef>::Args as clap::FromArgMatches>::from_arg_matches(&m)
                        .map_err(|e| ::anyhow::anyhow!("{e}"))?;
                    let $out: <$tool as OrcaToolDef>::Output = if let Some(peer) = peer {
                        $crate::cli::exec_remote::<$tool>(&peer, args, &ctx).await?
                    } else {
                        <$tool as OrcaTool>::run(args, &ctx).await?
                    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Command;

    fn flat_root() -> Command {
        Command::new("orca").subcommand(
            Command::new("engine")
                .subcommand_required(true)
                .subcommand(Command::new("list")),
        )
    }

    fn nested_root() -> Command {
        Command::new("orca").subcommand(
            Command::new("pod")
                .subcommand_required(true)
                .subcommand(
                    Command::new("peer")
                        .subcommand_required(true)
                        .subcommand(Command::new("list")),
                )
                .subcommand(Command::new("list")), // pod.list lives alongside pod.peer.*
        )
    }

    #[test]
    fn walk_to_verb_flat_domain() {
        let m = flat_root().get_matches_from(["orca", "engine", "list"]);
        let (domain, verb, _) = walk_to_verb(&m).unwrap();
        assert_eq!(domain, "engine");
        assert_eq!(verb, "list");
    }

    #[test]
    fn walk_to_verb_nested_domain() {
        let m = nested_root().get_matches_from(["orca", "pod", "peer", "list"]);
        let (domain, verb, _) = walk_to_verb(&m).unwrap();
        assert_eq!(domain, "pod.peer");
        assert_eq!(verb, "list");
    }

    #[test]
    fn walk_to_verb_mixed_tree_resolves_shallow_verb() {
        // `pod.list` must still resolve when `pod.peer.*` exists as a sibling
        // branch under the same `pod` segment.
        let m = nested_root().get_matches_from(["orca", "pod", "list"]);
        let (domain, verb, _) = walk_to_verb(&m).unwrap();
        assert_eq!(domain, "pod");
        assert_eq!(verb, "list");
    }

    #[test]
    fn walk_to_verb_no_subcommand_returns_none() {
        let m = Command::new("orca")
            .subcommand(Command::new("engine"))
            .get_matches_from(["orca"]);
        assert!(walk_to_verb(&m).is_none());
    }
}
