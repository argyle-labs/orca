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
use quote::quote;
use syn::{DeriveInput, Ident, parse::Parse, parse::ParseStream};

#[derive(Default)]
pub(crate) struct PluginStructAttr {
    pub args: bool,
}

impl Parse for PluginStructAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut out = PluginStructAttr::default();
        if input.is_empty() {
            return Ok(out);
        }
        let idents = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated(input)?;
        for id in idents {
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
        Ok(out)
    }
}

pub(crate) fn expand(attr: PluginStructAttr, item: DeriveInput) -> TokenStream2 {
    let derives = if attr.args {
        quote! {
            #[derive(
                ::plugin_toolkit::clap::Args,
                ::plugin_toolkit::serde::Serialize,
                ::plugin_toolkit::serde::Deserialize,
                ::plugin_toolkit::schemars::JsonSchema,
                ::core::default::Default,
            )]
        }
    } else {
        quote! {
            #[derive(
                ::plugin_toolkit::serde::Serialize,
                ::plugin_toolkit::serde::Deserialize,
                ::plugin_toolkit::schemars::JsonSchema,
            )]
        }
    };

    quote! {
        #derives
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        #item
    }
}
