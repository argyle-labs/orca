//! Catalog-driven plugin fetch (slice 5 of the plugin-externalization migration).
//!
//! Given a first-party catalog entry, resolve the GitHub **release asset that
//! matches THIS daemon's target triple** (arch + libc), download it, and
//! checksum-verify it. The caller ([`crate::plugin_manager`]) then runs the
//! normal `abi_stable` compat gate and registers the plugin live.
//!
//! ## Why derive the asset URL instead of storing it
//!
//! A plugin `.so` is coupled to three axes of the loading daemon:
//!
//! 1. **`plugin-abi` version** — the `abi_stable` layout/RootModule tag. Held
//!    stable across orca releases by pinning `plugin-abi` (see its Cargo.toml),
//!    so a plugin built against `plugin-abi 0.1.x` loads into any orca on 0.1.x.
//! 2. **libc** — a glibc `.so` cannot `dlopen` into a musl daemon, or vice
//!    versa. Encoded in the target triple (`…-linux-gnu` vs `…-linux-musl`).
//! 3. **arch** — `x86_64` vs `aarch64`.
//!
//! (2) and (3) live in the Rust target triple this binary was built for
//! ([`crate::update::build_target`]). The shared plugin release workflow
//! publishes one asset per triple under a deterministic name, so the download
//! URL is a pure function of `(repo, version, triple)` — no per-entry URL table
//! to drift. Asset name convention (MUST match the release workflow):
//!
//! ```text
//! {name}-v{version}-{triple}.{ext}
//! e.g. proxmox-v0.1.1-rc.2-x86_64-unknown-linux-gnu.so
//!      docker-v0.1.1-aarch64-unknown-linux-musl.so
//! ```

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::update;

/// GitHub REST API version pin (same as `update.rs`).
const GITHUB_API_VERSION: &str = "2022-11-28";
const ORCA_VERSION: &str = env!("ORCA_VERSION");

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    /// GitHub API asset URL (used for authenticated octet-stream download).
    url: String,
    /// Public direct-download URL (used unauthenticated when no token is set).
    #[serde(default)]
    browser_download_url: String,
}

/// A downloaded, checksum-verified plugin cdylib, ready to be gated + installed.
pub struct FetchedPlugin {
    pub bytes: Vec<u8>,
    /// Release version with any leading `v` stripped, e.g. `0.1.1-rc.2`.
    pub version: String,
    /// The asset filename that was resolved (for logging / error context).
    pub asset: String,
}

/// Convert a repo web URL (`https://github.com/OWNER/REPO`) to its REST API
/// base (`https://api.github.com/repos/OWNER/REPO`). `None` for non-github.com.
fn repo_api_base(repo_url: &str) -> Option<String> {
    let rest = repo_url
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")?;
    let mut it = rest.split('/');
    let owner = it.next()?;
    let repo = it.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("https://api.github.com/repos/{owner}/{repo}"))
}

/// cdylib extension for this platform, matching `plugin_manager::install_filename`.
fn dylib_ext() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "dylib"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "so"
    }
    #[cfg(windows)]
    {
        "dll"
    }
}

/// Deterministic release-asset filename for a plugin at a version+triple.
fn asset_name(name: &str, version: &str, triple: &str, ext: &str) -> String {
    format!("{name}-v{version}-{triple}.{ext}")
}

/// Download a public release asset without authentication (used when no
/// `github_token` is configured — release repos are public). Mirrors
/// [`update::download_asset`]'s size/timeout envelope.
async fn download_public(client: &utils::http::Client, url: &str) -> Result<Vec<u8>> {
    const MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
    let resp = client
        .get(url.to_string())
        .header("Accept", "application/octet-stream")
        .header("User-Agent", format!("orca/{ORCA_VERSION}"))
        .max_body(MAX_ASSET_BYTES)
        .timeout(std::time::Duration::from_secs(300))
        .send_bytes()
        .await
        .context("public asset download failed")?;
    Ok(resp.body)
}

/// Resolve + download the plugin release asset matching this daemon's target.
///
/// * `name` — catalog name (also the cdylib `target_software` and asset prefix).
/// * `repo_url` — the catalog entry's `repoUrl`.
/// * `version` — explicit version/tag, or `None` for the newest release.
/// * `allow_prerelease` — when `None` version: include `-rc` tags (newest wins)
///   rather than only the stable `/releases/latest`.
pub async fn fetch(
    name: &str,
    repo_url: &str,
    version: Option<&str>,
    allow_prerelease: bool,
) -> Result<FetchedPlugin> {
    let api = repo_api_base(repo_url)
        .with_context(|| format!("catalog repoUrl is not a github.com URL: {repo_url}"))?;
    let triple = update::build_target();
    if triple == "unknown-target" {
        bail!(
            "this daemon has no baked build target (ORCA_BUILD_TARGET unset); \
             cannot resolve a matching plugin asset"
        );
    }
    let ext = dylib_ext();
    let token = update::resolve_github_token(); // empty is OK for public repos

    let client = utils::http::Client::new();
    let ua = format!("orca/{ORCA_VERSION}");
    let get = |url: String| {
        let mut r = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header("User-Agent", &ua);
        if !token.is_empty() {
            r = r.bearer(&token);
        }
        r
    };

    // Resolve the release: explicit tag, or newest (stable-only unless prerelease).
    let release: Release = match version {
        Some(v) => {
            let v_tag = if v.starts_with('v') {
                v.to_string()
            } else {
                format!("v{v}")
            };
            get(format!("{api}/releases/tags/{v_tag}"))
                .send()
                .await
                .with_context(|| format!("fetch release {v_tag} from {api}"))?
                .json()
                .context("parse release json")?
        }
        None if allow_prerelease => {
            let list: Vec<Release> = get(format!("{api}/releases?per_page=30"))
                .send()
                .await
                .with_context(|| format!("list releases from {api}"))?
                .json()
                .context("parse releases json")?;
            list.into_iter()
                .next()
                .with_context(|| format!("no releases published for {name}"))?
        }
        None => get(format!("{api}/releases/latest"))
            .send()
            .await
            .with_context(|| format!("fetch latest release from {api}"))?
            .json()
            .context("parse release json")?,
    };

    let resolved = release.tag_name.trim_start_matches('v').to_string();
    let want = asset_name(name, &resolved, triple, ext);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == want)
        .with_context(|| {
            format!(
                "release {} has no asset '{want}' for this target ({triple}) — \
             the plugin may not publish a {triple} build yet",
                release.tag_name
            )
        })?;

    let fetch_one = |a: &Asset| {
        let (url, browser, tok) = (a.url.clone(), a.browser_download_url.clone(), token.clone());
        let client = &client;
        async move {
            if !tok.is_empty() {
                update::download_asset(client, &url, &tok).await
            } else if !browser.is_empty() {
                download_public(client, &browser).await
            } else {
                bail!("no download URL for asset and no github token to use the API URL")
            }
        }
    };

    let bytes = fetch_one(asset).await?;

    // Verify against a sibling `<asset>.sha256` if the release ships one.
    if let Some(cs) = release
        .assets
        .iter()
        .find(|a| a.name == format!("{want}.sha256"))
    {
        let cs_bytes = fetch_one(cs).await?;
        let expected = String::from_utf8_lossy(&cs_bytes)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if expected.is_empty() {
            bail!("checksum asset {want}.sha256 is empty");
        }
        update::verify_sha256(&bytes, &expected)?;
    } else {
        tracing::warn!(
            plugin = %name,
            asset = %want,
            "no .sha256 checksum asset published for this release; installing without integrity check"
        );
    }

    Ok(FetchedPlugin {
        bytes,
        version: resolved,
        asset: want,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_api_base_maps_github_url() {
        assert_eq!(
            repo_api_base("https://github.com/argyle-labs/proxmox").as_deref(),
            Some("https://api.github.com/repos/argyle-labs/proxmox")
        );
        assert_eq!(
            repo_api_base("https://github.com/argyle-labs/proxmox/").as_deref(),
            Some("https://api.github.com/repos/argyle-labs/proxmox")
        );
    }

    #[test]
    fn repo_api_base_rejects_non_github() {
        assert_eq!(repo_api_base("https://gitlab.com/x/y"), None);
        assert_eq!(repo_api_base("https://github.com/only-owner"), None);
    }

    #[test]
    fn asset_name_matches_release_convention() {
        assert_eq!(
            asset_name("proxmox", "0.1.1-rc.2", "x86_64-unknown-linux-gnu", "so"),
            "proxmox-v0.1.1-rc.2-x86_64-unknown-linux-gnu.so"
        );
    }
}
