pub mod openapi;

#[cfg(test)]
mod golden;

use std::path::{Path, PathBuf};

use ::openapi::surface::ApiSurface;
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
