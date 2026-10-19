//! Golden check against the full vendored specs (run after any spec bump):
//! `ORCA_GOLDEN_PROXMOX_SPECS=../proxmox/specs ORCA_GOLDEN_GITEA_SPECS=../gitea/specs
//! cargo test -p orca-plugin-toolkit-build --lib full_specs -- --ignored`
//! and `ORCA_GOLDEN_UNRAID=../unraid cargo test -p orca-plugin-toolkit-build --lib full_unraid -- --ignored`.

pub mod graphql;
pub mod openapi;

#[cfg(test)]
mod golden;
#[cfg(test)]
mod golden_graphql;

use std::path::{Path, PathBuf};

use ::openapi::surface::{ApiSurface, Policy};
use anyhow::{Context, Result, bail};

pub fn write(surface: &ApiSurface, out_dir: &Path) -> Result<PathBuf> {
    let text = serde_json::to_string(surface).context("serialize api surface")?;
    if text.len() > plugin_proto::MAX_API_SURFACE_BYTES {
        bail!(
            "api surface `{}` is {} bytes, over the {} byte cap",
            surface.surface,
            text.len(),
            plugin_proto::MAX_API_SURFACE_BYTES
        );
    }
    ApiSurface::parse(&text)
        .with_context(|| format!("api surface `{}` does not round-trip", surface.surface))?;
    let path = out_dir.join(format!("{}.api-surface.json", surface.surface));
    std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
    println!(
        "cargo:warning=surface_table[{}]: {} op(s), {} def(s), {} bytes",
        surface.surface,
        surface.ops.len(),
        surface.defs.len(),
        text.len()
    );
    Ok(path)
}

pub(crate) fn derived_policy(
    tail: &str,
    is_write: bool,
    role: &str,
    data_mutation: bool,
) -> Policy {
    let read_shaped = verb_shape::is_read_shaped(tail);
    let role = match role {
        "" if read_shaped => "read",
        "" => "admin",
        declared => declared,
    };
    Policy {
        role: role.to_string(),
        execute_gated: is_write || !read_shaped,
        data_mutation,
        local_only: false,
    }
}
