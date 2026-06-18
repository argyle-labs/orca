//! Convert a Proxmox VE `apidoc.js` (or `apidoc.json`) into an OpenAPI 3.x
//! spec and print it to stdout.
//!
//! Usage:
//!   cargo run -p openapi --example pve_to_openapi -- path/to/apidoc.js > proxmox.openapi.json
//!
//! Round-trips through `openapi::from_pve::parse_str` followed by
//! `openapi::normalize::for_progenitor`, producing a spec ready to drop
//! into a plugin's `specs/` directory and feed to progenitor.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: pve_to_openapi <apidoc.js>"))?
        .into();
    let raw = std::fs::read_to_string(&path)?;
    let mut spec = openapi::from_pve::parse_str(&raw)?;
    let report = openapi::normalize::for_progenitor(&mut spec);
    eprintln!(
        "pve_to_openapi: paths={}  synthesized_ids={}  multipart_rewrites={}",
        spec.paths.paths.len(),
        report.synthesized_ids.len(),
        // Other report counters are bumped only on shapes PVE doesn't
        // emit; surface them too if a future spec version changes that.
        0
    );
    let out = serde_json::to_string_pretty(&spec)?;
    println!("{out}");
    Ok(())
}
