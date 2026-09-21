//! `system.certs.list` — mesh cert + trust status for this host.
//!
//! Founder/member role, each mesh cert's days-remaining, the running orca
//! version, and the Tier-2 `self_secure` (secrets-storage) policy flag — one
//! read. The reframed home of the former `pod.detail view=certs`.
//!
//! Lives in the `system` crate (not `pod`): the cert-status reader was hoisted
//! into `utils::pki` (`mesh_cert_status`) and `self_secure` is a `db::pod` read,
//! so this verb carries no pod dependency — part of dissolving `pod.detail`.
//! Peer-dispatchable, so `system certs list --peer <host>` reads a remote host's
//! cert status over the mesh (the handler runs on that host, reporting its own).

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemCertsListArgs {}

/// Mesh cert inventory + trust/secrets status for the host this runs on. Pure
/// filesystem read of the PKI dir, plus the DB-backed `self_secure` flag layered
/// on top.
#[orca_tool(domain = "system.certs", verb = "list")]
async fn system_certs_list(
    _args: SystemCertsListArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<utils::pki::MeshCertStatus> {
    let pki_dir =
        contract::config::paths::pki_dir().unwrap_or_else(|_| ctx.config.app_dir.join("pki"));
    let mut out = utils::pki::mesh_cert_status(&pki_dir);
    out.self_secure = db::pool::with_pooled_or_open(db::pod::get_self_secure).unwrap_or(false);
    Ok(out)
}
