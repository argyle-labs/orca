//! Catalog-driven plugin fetch (slice 5 of the plugin-externalization migration).
//!
//! Given a first-party catalog entry, resolve the GitHub **release asset that
//! matches THIS daemon's target triple** (arch + libc), download it, and
//! checksum-verify it. The caller ([`crate::plugin_manager`]) then spawns the
//! plugin, completes the `plugin-proto` handshake, and registers it live.
//!
//! ## Why derive the asset URL instead of storing it
//!
//! A plugin executable is coupled to two axes of the loading daemon:
//!
//! 1. **libc** — a glibc binary cannot run against a musl daemon's dynamic
//!    loader, or vice versa. Encoded in the target triple (`…-linux-gnu` vs
//!    `…-linux-musl`).
//! 2. **arch** — `x86_64` vs `aarch64`.
//!
//! Both live in the Rust target triple this binary was built for
//! ([`crate::update::build_target`]). Runtime protocol compatibility is a
//! separate, dynamically-negotiated concern (the `plugin-proto` major match at
//! spawn), not an artifact-naming axis. The shared plugin release workflow
//! publishes one executable per triple under a deterministic name, so the
//! download URL is a pure function of `(repo, version, triple)` — no per-entry
//! URL table to drift. Asset name convention (MUST match the release workflow):
//!
//! ```text
//! {name}-v{version}-{triple}
//! e.g. proxmox-v0.1.1-rc.2-x86_64-unknown-linux-gnu
//!      docker-v0.1.1-aarch64-unknown-linux-musl
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

/// A downloaded, checksum-verified plugin executable, ready to be installed.
pub struct FetchedPlugin {
    pub bytes: Vec<u8>,
    /// Release version with any leading `v` stripped, e.g. `0.1.1-rc.2`.
    pub version: String,
    /// The asset filename that was resolved (for logging / error context).
    pub asset: String,
    /// Hex sha256 of `bytes`: the digest verified against the release's
    /// published `<asset>.sha256` blob when present, else computed from the
    /// downloaded bytes. Callers delegating a fetch across the mesh re-verify
    /// this after base64 transit. See [[github-token-proxy-delegate-on-miss]].
    pub sha256: String,
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

/// Deterministic release-asset filename for a plugin executable at a
/// version+triple (a bare executable — no extension).
fn asset_name(name: &str, version: &str, triple: &str) -> String {
    format!("{name}-v{version}-{triple}")
}

/// Normalize a user-supplied version to a git tag: ensure exactly one leading
/// `v` (`0.1.1` → `v0.1.1`, `v0.1.1` → `v0.1.1`).
fn version_to_tag(v: &str) -> String {
    if v.starts_with('v') {
        v.to_string()
    } else {
        format!("v{v}")
    }
}

/// The checksum value from a `<asset>.sha256` file's contents: the first
/// whitespace-delimited token (handles both bare-hash and `HASH  filename`
/// formats). Empty when the file has no token.
fn checksum_token(contents: &[u8]) -> String {
    String::from_utf8_lossy(contents)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
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
/// * `name` — catalog name (also the plugin's `target_software` and asset prefix).
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
    let triple = update::build_target();
    if triple == "unknown-target" {
        bail!(
            "this daemon has no baked build target (ORCA_BUILD_TARGET unset); \
             cannot resolve a matching plugin asset"
        );
    }
    fetch_for_target(name, repo_url, version, allow_prerelease, triple).await
}

/// Like [`fetch`], but resolves the asset for an **explicit** target triple
/// rather than this daemon's own. This is what lets a token-holding peer serve
/// a plugin asset for a requester on a different arch/libc: the holder runs
/// this with the *caller's* triple + the holder's own GitHub token. See
/// [`crate::plugin_manager`]'s `plugin.serve_asset` and
/// [[github-token-proxy-delegate-on-miss]].
pub async fn fetch_for_target(
    name: &str,
    repo_url: &str,
    version: Option<&str>,
    allow_prerelease: bool,
    triple: &str,
) -> Result<FetchedPlugin> {
    let api = repo_api_base(repo_url)
        .with_context(|| format!("catalog repoUrl is not a github.com URL: {repo_url}"))?;
    if triple == "unknown-target" {
        bail!("empty/unknown target triple; cannot resolve a matching plugin asset");
    }
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
            let v_tag = version_to_tag(v);
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

    // Resolve the asset via orca core's single-source-of-truth candidate order
    // (see `release_targets`): on linux, prefer the musl-static asset (runs on
    // both musl and glibc hosts) and fall back to the gnu asset for the same
    // arch; on darwin, the daemon's own triple is the only candidate. We take
    // the first candidate this release actually ships.
    let candidates = crate::release_targets::linux_asset_candidates(triple);
    if candidates.is_empty() {
        bail!("no release-asset candidate for daemon target '{triple}'");
    }
    let (want, asset) = candidates
        .iter()
        .find_map(|cand| {
            let n = asset_name(name, &resolved, cand);
            release.assets.iter().find(|a| a.name == n).map(|a| (n, a))
        })
        .with_context(|| {
            format!(
                "release {} has none of the candidate assets {:?} for this host \
                 ({triple}) — the plugin may not publish a matching build yet",
                release.tag_name,
                candidates
                    .iter()
                    .map(|c| asset_name(name, &resolved, c))
                    .collect::<Vec<_>>(),
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

    // Verify against a sibling `<asset>.sha256` if the release ships one. The
    // resulting digest is returned so a delegated (peer-served) fetch can be
    // re-verified after base64 transit on the requesting host.
    let sha256 = if let Some(cs) = release
        .assets
        .iter()
        .find(|a| a.name == format!("{want}.sha256"))
    {
        let cs_bytes = fetch_one(cs).await?;
        let expected = checksum_token(&cs_bytes);
        if expected.is_empty() {
            bail!("checksum asset {want}.sha256 is empty");
        }
        update::verify_sha256(&bytes, &expected)?;
        expected
    } else {
        tracing::warn!(
            plugin = %name,
            asset = %want,
            "no .sha256 checksum asset published for this release; installing without integrity check"
        );
        utils::hash::sha256_hex(&bytes)
    };

    Ok(FetchedPlugin {
        bytes,
        version: resolved,
        asset: want,
        sha256,
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
    fn repo_api_base_trims_trailing_and_deep_paths() {
        // Deeper paths keep only owner/repo.
        assert_eq!(
            repo_api_base("https://github.com/argyle-labs/proxmox/tree/main").as_deref(),
            Some("https://api.github.com/repos/argyle-labs/proxmox")
        );
        // Empty owner or repo → None.
        assert_eq!(repo_api_base("https://github.com//repo"), None);
        assert_eq!(repo_api_base("https://github.com/owner/"), None);
    }

    #[test]
    fn asset_name_matches_release_convention() {
        assert_eq!(
            asset_name("proxmox", "0.1.1-rc.2", "x86_64-unknown-linux-gnu"),
            "proxmox-v0.1.1-rc.2-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            asset_name("docker", "0.1.1", "aarch64-unknown-linux-musl"),
            "docker-v0.1.1-aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn version_to_tag_ensures_single_v_prefix() {
        assert_eq!(version_to_tag("0.1.1"), "v0.1.1");
        assert_eq!(version_to_tag("v0.1.1"), "v0.1.1");
        assert_eq!(version_to_tag("0.1.1-rc.2"), "v0.1.1-rc.2");
    }

    #[test]
    fn checksum_token_takes_first_field() {
        // Bare hash.
        assert_eq!(checksum_token(b"abc123\n"), "abc123");
        // `HASH  filename` (sha256sum output format).
        assert_eq!(
            checksum_token(b"deadbeef  proxmox-v0.1.1-x86_64.so\n"),
            "deadbeef"
        );
        // Empty / whitespace-only → empty token.
        assert_eq!(checksum_token(b""), "");
        assert_eq!(checksum_token(b"   \n"), "");
    }

    // ── release / asset deserialization ───────────────────────────────────────

    #[test]
    fn release_deserializes_with_assets() {
        let json = r#"{
            "tag_name": "v0.1.1-rc.2",
            "assets": [
                {
                    "name": "proxmox-v0.1.1-rc.2-x86_64-unknown-linux-gnu.so",
                    "url": "https://api.github.com/repos/x/y/releases/assets/1",
                    "browser_download_url": "https://github.com/x/y/releases/download/v0.1.1-rc.2/a.so"
                }
            ]
        }"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(r.tag_name, "v0.1.1-rc.2");
        assert_eq!(r.assets.len(), 1);
        assert!(r.assets[0].url.contains("assets/1"));
        assert!(r.assets[0].browser_download_url.contains("download"));
    }

    #[test]
    fn release_defaults_missing_assets_to_empty() {
        let r: Release = serde_json::from_str(r#"{"tag_name":"v0.1.0"}"#).unwrap();
        assert!(r.assets.is_empty());
    }

    #[test]
    fn asset_defaults_missing_browser_url() {
        let json = r#"{"name":"a.so","url":"https://api/x"}"#;
        let a: Asset = serde_json::from_str(json).unwrap();
        assert_eq!(a.name, "a.so");
        assert!(a.browser_download_url.is_empty());
    }

    #[test]
    fn tag_name_strips_leading_v_for_resolved_version() {
        // Mirrors `fetch`'s `release.tag_name.trim_start_matches('v')`.
        let r: Release = serde_json::from_str(r#"{"tag_name":"v0.1.1-rc.2"}"#).unwrap();
        assert_eq!(r.tag_name.trim_start_matches('v'), "0.1.1-rc.2");
    }

    #[test]
    fn asset_selection_finds_matching_triple() {
        // Reproduces the `assets.iter().find(name == want)` selection in `fetch`.
        let want = asset_name("proxmox", "0.1.1", "x86_64-unknown-linux-gnu");
        let names = [
            "proxmox-v0.1.1-aarch64-unknown-linux-gnu".to_string(),
            want.clone(),
            format!("{want}.sha256"),
        ];
        assert!(names.contains(&want));
        assert!(names.contains(&format!("{want}.sha256")));
        // A triple with no published asset is absent.
        let missing = asset_name("proxmox", "0.1.1", "riscv64-unknown-linux-gnu");
        assert!(!names.contains(&missing));
    }

    // ── fetch_for_target: pre-network error branches ──────────────────────────

    #[tokio::test]
    async fn fetch_for_target_rejects_non_github_repo() {
        // A non-github.com repoUrl fails at `repo_api_base` before any network
        // call, with the catalog-context error message.
        let res = fetch_for_target(
            "proxmox",
            "https://gitlab.com/argyle-labs/proxmox",
            None,
            false,
            "x86_64-unknown-linux-gnu",
        )
        .await;
        let Err(err) = res else {
            panic!("non-github repoUrl must fail");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a github.com URL"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("gitlab.com"), "should echo the bad url: {msg}");
    }

    #[tokio::test]
    async fn fetch_for_target_rejects_unknown_triple() {
        // A valid github repoUrl but an `unknown-target` triple bails after
        // resolving the api base but before any release fetch.
        let res = fetch_for_target(
            "proxmox",
            "https://github.com/argyle-labs/proxmox",
            None,
            false,
            "unknown-target",
        )
        .await;
        let Err(err) = res else {
            panic!("unknown triple must fail");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown target triple"),
            "unexpected error: {msg}"
        );
    }

    // ── download_public (mocked HTTP) ─────────────────────────────────────────
    mod download {
        use super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn download_public_returns_body_bytes() {
            let server = MockServer::start().await;
            let payload = b"\x7fELF-plugin-binary-bytes".to_vec();
            Mock::given(method("GET"))
                .and(path("/asset.bin"))
                .and(header("Accept", "application/octet-stream"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
                .mount(&server)
                .await;
            let client = utils::http::Client::new();
            let bytes = download_public(&client, &format!("{}/asset.bin", server.uri()))
                .await
                .expect("public download of a 200 body should succeed");
            assert_eq!(bytes, payload);
        }

        #[tokio::test]
        async fn download_public_sends_orca_user_agent() {
            // The request must carry a `orca/<version>` User-Agent; a matcher on
            // it proves the header is set (mock only replies when it matches).
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/asset.bin"))
                .and(header(
                    "User-Agent",
                    format!("orca/{ORCA_VERSION}").as_str(),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
                .mount(&server)
                .await;
            let client = utils::http::Client::new();
            let bytes = download_public(&client, &format!("{}/asset.bin", server.uri()))
                .await
                .expect("request with orca UA should match the mock and succeed");
            assert_eq!(bytes, b"ok");
        }

        #[tokio::test]
        async fn download_public_errors_on_unreachable_host() {
            // Port 1 refuses connections: the download fails with the wrapping
            // "public asset download failed" context rather than hanging.
            let client = utils::http::Client::new();
            let res = download_public(&client, "http://127.0.0.1:1/asset.bin").await;
            let Err(err) = res else {
                panic!("connection to a refused port must fail");
            };
            let msg = format!("{err:#}");
            assert!(
                msg.contains("public asset download failed"),
                "unexpected error: {msg}"
            );
        }
    }
}
