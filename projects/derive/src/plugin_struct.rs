//! `#[plugin_struct]` attribute macro.
//!
//! Injects the derive set + crate-path attributes plugin authors would
//! otherwise hand-write boilerplate for, with all paths anchored to
//! `::plugin_toolkit::*` so the plugin's Cargo.toml does NOT need direct
//! deps on `serde`, `schemars`, or `clap`.
//!
//! Flags:
//!   - bare `#[plugin_struct]`        → Serialize + Deserialize + JsonSchema
//!   - `#[plugin_struct(args)]`       → above + clap::Args + Default
//!   - `#[plugin_struct(output)]`     → Serialize + Deserialize + JsonSchema
//!     (alias for the bare form; explicit for tool output structs)
//!   - `#[plugin_struct(crate = ::macro_runtime)]` → anchor emitted paths
//!     against `::macro_runtime` instead of the default `::plugin_toolkit`.
//!     Used by domain crates that can't depend on `plugin-toolkit` without
//!     creating a Cargo cycle.
//!
//! Expands to:
//!
//! ```rust,ignore
//! #[derive(
//!     ::plugin_toolkit::serde::Serialize,
//!     ::plugin_toolkit::serde::Deserialize,
//!     ::plugin_toolkit::schemars::JsonSchema,
//! )]
//! #[serde(crate = "::plugin_toolkit::serde")]
//! #[schemars(crate = "::plugin_toolkit::schemars")]
//! pub struct Foo { … }
//! ```
//!
//! `clap::Args` and `Default` are added only for `args` flavor. clap has
//! no `crate = ...` attribute so it must be reachable as the path
//! `::plugin_toolkit::clap`, which it is via toolkit's re-export.

use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{DeriveInput, Expr, Ident, parse::Parse, parse::ParseStream};

pub(crate) struct PluginStructAttr {
    pub args: bool,
    pub crate_path: syn::Path,
}

impl Default for PluginStructAttr {
    fn default() -> Self {
        Self {
            args: false,
            crate_path: syn::parse_quote!(::plugin_toolkit),
        }
    }
}

impl Parse for PluginStructAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut out = PluginStructAttr::default();
        if input.is_empty() {
            return Ok(out);
        }
        // Two grammars share this attr: bare idents (`args`, `output`) and
        // `key = value` pairs (currently only `crate = ::path`). Parse a flat
        // comma-separated list and dispatch per item.
        let metas =
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated(input)?;
        for m in metas {
            match m {
                syn::Meta::Path(p) => {
                    let id: Ident = p.get_ident().cloned().ok_or_else(|| {
                        syn::Error::new_spanned(&p, "expected an identifier flag")
                    })?;
                    match id.to_string().as_str() {
                        "args" => out.args = true,
                        "output" => { /* alias for default */ }
                        other => {
                            return Err(syn::Error::new(
                                id.span(),
                                format!("unknown plugin_struct flag '{other}' (want: args|output)"),
                            ));
                        }
                    }
                }
                syn::Meta::NameValue(nv) => {
                    let key = nv
                        .path
                        .get_ident()
                        .map(|i| i.to_string())
                        .unwrap_or_default();
                    match key.as_str() {
                        "crate" => match &nv.value {
                            Expr::Path(p) => out.crate_path = p.path.clone(),
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    &nv.value,
                                    "expected a path (e.g. `::plugin_toolkit` or `::macro_runtime`)",
                                ));
                            }
                        },
                        other => {
                            return Err(syn::Error::new_spanned(
                                &nv.path,
                                format!("unknown plugin_struct key '{other}'"),
                            ));
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `args`, `output`, or `crate = ::path`",
                    ));
                }
            }
        }
        Ok(out)
    }
}

pub(crate) fn expand(attr: PluginStructAttr, item: DeriveInput) -> TokenStream2 {
    let crate_path = &attr.crate_path;
    // `#[serde(crate = "...")]` / `#[schemars(crate = "...")]` take a string
    // literal, not a path token. Stringify by rendering the path's tokens.
    let crate_path_str = crate_path.to_token_stream().to_string().replace(' ', "");
    let serde_path = format!("{crate_path_str}::serde");
    let schemars_path = format!("{crate_path_str}::schemars");

    let derives = if attr.args {
        quote! {
            #[derive(
                #crate_path::clap::Args,
                #crate_path::serde::Serialize,
                #crate_path::serde::Deserialize,
                #crate_path::schemars::JsonSchema,
                ::core::default::Default,
            )]
        }
    } else {
        quote! {
            #[derive(
                #crate_path::serde::Serialize,
                #crate_path::serde::Deserialize,
                #crate_path::schemars::JsonSchema,
            )]
        }
    };

    quote! {
        #derives
        #[serde(crate = #serde_path)]
        #[schemars(crate = #schemars_path)]
        #item
    }
}
