//! `openapi_plugin` — the single declarative front door for a full OpenAPI plugin.
//!
//! An API plugin that covers an upstream's OpenAPI surface used to hand-wire the
//! same four things, none of them enforced — miss one and it breaks in a
//! non-obvious way:
//!   1. `Cargo.toml`: `default-features = false` + the `delegated-http` feature
//!      set (so the generated client rides orca's `http.request` capability and
//!      links no reqwest/rustls);
//!   2. `build.rs`: two ordered codegen calls (client, then tool surface);
//!   3. a hand-written `crate::tools::make_client` whose *exact* signature the
//!      surface generator hard-codes;
//!   4. an `endpoint_resource!` registry + the auth-header wiring.
//!
//! `openapi_plugin` collapses 2–4 into one call. The plugin declares its spec,
//! orca domain, and auth scheme; the front door runs both codegen passes and
//! *emits* the `endpoint_resource!` registry + the `make_client` the surface
//! expects — derived from the declared [`Auth`]. Paired with the toolkit's
//! `openapi-plugin` meta-feature (which fixes #1 to a single feature name), a
//! fully-covered plugin becomes a spec file plus a handful of lines, with no
//! hand-written client to break under `delegated-http` ever again.
//!
//! ```rust,ignore
//! // build.rs — the entire plugin build
//! plugin_toolkit_build::openapi_plugin(OpenApiPlugin {
//!     domain: "jellyfin",
//!     spec: "specs/jellyfin-openapi-12.0.0.json",
//!     keep_paths: &[],                 // empty = the full API surface
//!     auth: Auth::header("authorization", r#"MediaBrowser Token="{token}""#),
//! })?;
//! ```
//!
//! The plugin then stitches the three emitted files (all under `OUT_DIR`) with:
//! ```rust,ignore
//! pub mod generated { include!(concat!(env!("OUT_DIR"), "/jellyfin_codegen.rs")); }
//! pub mod surface   { include!(concat!(env!("OUT_DIR"), "/jellyfin_surface.rs")); }
//! pub mod tools     { include!(concat!(env!("OUT_DIR"), "/jellyfin_wiring.rs")); /* + specials */ }
//! ```
//! and layers any hand-written specials (diagnosis tools, custom analysis) on
//! top of `crate::generated::Client`.

use std::path::Path;

use anyhow::{Context, Result};

use crate::{openapi, surface};

/// How the generated `make_client` authenticates each request. The template
/// forms produce a single header whose value the endpoint's `token` fills in.
pub enum Auth<'a> {
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// A single header. `value_template` must contain `{token}`, replaced with
    /// the endpoint's stored token at call time — e.g.
    /// `Auth::header("authorization", r#"MediaBrowser Token="{token}""#)`.
    Header {
        name: &'a str,
        value_template: &'a str,
    },
    /// No auth header (public API). `token` is still stored but unused.
    None,
}

impl<'a> Auth<'a> {
    /// Convenience constructor for [`Auth::Header`].
    pub fn header(name: &'a str, value_template: &'a str) -> Self {
        Auth::Header {
            name,
            value_template,
        }
    }

    /// `(header_name, value_expr)` for the generated `make_client`, or `None`
    /// for [`Auth::None`]. `value_expr` is a Rust expression string that
    /// formats the header value from the in-scope `row.token`.
    fn header_wiring(&self) -> Option<(String, String)> {
        match self {
            Auth::Bearer => Some((
                "authorization".to_string(),
                "format!(\"Bearer {}\", row.token)".to_string(),
            )),
            Auth::Header {
                name,
                value_template,
            } => {
                // Turn the `{token}` template into a `format!` call. `{` / `}`
                // that are not the token placeholder are escaped for format!.
                let fmt = value_template
                    .replace('{', "{{")
                    .replace('}', "}}")
                    .replace("{{token}}", "{}");
                Some((name.to_string(), format!("format!({fmt:?}, row.token)")))
            }
            Auth::None => None,
        }
    }
}

/// The declaration a plugin's `build.rs` hands to [`openapi_plugin`].
pub struct OpenApiPlugin<'a> {
    /// orca domain **and** the generated module/flavor name — must be a valid
    /// Rust identifier (no hyphens), since it names the `generated`/`surface`
    /// modules and the `#[orca_tool(domain = …)]` on every emitted tool.
    pub domain: &'a str,
    /// Path to the vendored spec file (its directory is scanned for the
    /// `x-orca-user-callable` surface exceptions).
    pub spec: &'a str,
    /// Restrict codegen to these paths (empty = the whole spec = full API).
    pub keep_paths: &'a [&'a str],
    /// How the generated `make_client` authenticates.
    pub auth: Auth<'a>,
}

/// Run the full OpenAPI-plugin build: client codegen, tool-surface codegen, and
/// the emitted `endpoint_resource!` registry + `make_client` wiring. Call from
/// `build.rs`. Writes `<OUT_DIR>/<domain>_{codegen,surface,wiring}.rs`.
pub fn openapi_plugin(cfg: OpenApiPlugin<'_>) -> Result<()> {
    let out_dir = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .context("OUT_DIR not set — openapi_plugin must be called from build.rs")?;
    let spec_path = Path::new(cfg.spec);
    let specs_dir = spec_path
        .parent()
        .context("spec path has no parent directory")?;

    // 1. Progenitor client → <domain>_codegen.rs
    openapi::generate_one(spec_path, cfg.domain, cfg.domain, cfg.keep_paths)?;
    // 2. Tool surface (reads the codegen file, anchors JsonSchema, emits tools).
    surface::openapi::generate(specs_dir, &out_dir, cfg.domain)?;
    // 3. endpoint_resource! registry + make_client, derived from the auth model.
    let wiring = emit_wiring(cfg.domain, &cfg.auth);
    let wiring_path = out_dir.join(format!("{}_wiring.rs", cfg.domain));
    std::fs::write(&wiring_path, wiring)
        .with_context(|| format!("write {}", wiring_path.display()))?;
    println!(
        "cargo:warning=openapi_plugin[{}]: client + surface + wiring emitted",
        cfg.domain
    );
    Ok(())
}

/// Emit the `endpoint_resource!` row struct + the async `make_client` the
/// surface generator calls, for domain `domain` under auth scheme `auth`.
fn emit_wiring(domain: &str, auth: &Auth<'_>) -> String {
    let struct_ident = format!("{}Endpoint", pascal(domain));
    let client_build = match auth.header_wiring() {
        Some((name, value_expr)) => format!(
            "    let http = plugin_toolkit::api_client::ApiClientBuilder::new()\n\
             \x20       .header({name:?}, {value_expr})?\n\
             \x20       .build()?;\n\
             \x20   Ok(crate::generated::Client::new_with_client(&row.base_url, http))"
        ),
        None => "    let http = plugin_toolkit::api_client::ApiClientBuilder::new().build()?;\n\
                 \x20   Ok(crate::generated::Client::new_with_client(&row.base_url, http))"
            .to_string(),
    };

    format!(
        "// @generated by plugin_toolkit_build::openapi_plugin — do not edit.\n\
         use plugin_toolkit::prelude::*;\n\n\
         /// Endpoint registry: `{domain}.{{list, detail, create, update, delete}}`\n\
         /// — the row struct, db helpers, schema fragment, and five CRUD tools,\n\
         /// generated wholesale by `#[endpoint_resource]`.\n\
         #[endpoint_resource(plugin = {domain:?})]\n\
         pub struct {struct_ident} {{\n\
         \x20   pub name: String,\n\
         \x20   pub base_url: String,\n\
         \x20   #[secret]\n\
         \x20   pub token: String,\n\
         \x20   pub enabled: bool,\n\
         }}\n\n\
         /// Resolve a registered `{domain}` endpoint to a ready generated client,\n\
         /// with the declared auth header pre-attached. Called by every surface\n\
         /// tool and by any hand-written special.\n\
         pub(crate) async fn make_client(name: &str) -> Result<crate::generated::Client> {{\n\
         \x20   let row = endpoint_db::get(name)?\n\
         \x20       .with_context(|| format!(\"{domain} endpoint '{{name}}' not registered\"))?;\n\
         \x20   if !row.enabled {{\n\
         \x20       bail!(\"{domain} endpoint '{{name}}' is disabled\");\n\
         \x20   }}\n\
         {client_build}\n\
         }}\n"
    )
}

/// `home_assistant` / `home-assistant` → `HomeAssistant`.
fn pascal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            cap = true;
        } else if cap {
            out.extend(c.to_uppercase());
            cap = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_wiring() {
        let (name, val) = Auth::Bearer.header_wiring().unwrap();
        assert_eq!(name, "authorization");
        assert_eq!(val, "format!(\"Bearer {}\", row.token)");
    }

    #[test]
    fn header_template_becomes_format_call() {
        let (name, val) = Auth::header("authorization", r#"MediaBrowser Token="{token}""#)
            .header_wiring()
            .unwrap();
        assert_eq!(name, "authorization");
        // {token} → {}, the literal quotes survive, result is a valid format! arg.
        assert!(val.starts_with("format!("));
        assert!(val.contains("MediaBrowser Token="));
        assert!(val.contains("{}"));
        assert!(val.ends_with(", row.token)"));
    }

    #[test]
    fn none_auth_has_no_header() {
        assert!(Auth::None.header_wiring().is_none());
    }

    #[test]
    fn wiring_emits_registry_and_make_client() {
        let out = emit_wiring("jellyfin", &Auth::Bearer);
        assert!(out.contains("#[endpoint_resource(plugin = \"jellyfin\")]"));
        assert!(out.contains("pub struct JellyfinEndpoint"));
        assert!(
            out.contains("async fn make_client(name: &str) -> Result<crate::generated::Client>")
        );
        assert!(out.contains("Bearer {}"));
        assert!(out.contains("new_with_client(&row.base_url, http)"));
    }

    #[test]
    fn pascal_handles_hyphen_and_underscore() {
        assert_eq!(pascal("home-assistant"), "HomeAssistant");
        assert_eq!(pascal("jellyfin"), "Jellyfin");
    }
}
