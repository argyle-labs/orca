//! `#[plugin_struct]` attribute macro.
//!
//! Injects the derive set + crate-path attributes plugin authors would
//! otherwise hand-write boilerplate for, with all paths anchored to
//! `::plugin_toolkit::*` so the plugin's Cargo.toml does NOT need direct
//! deps on `serde`, `schemars`, or `clap`.
//!
//! Flags:
//!   - bare `#[plugin_struct]`        → Serialize + Deserialize + JsonSchema
//!   - `#[plugin_struct(args)]` on a struct → above + clap::Args + Default
//!   - `#[plugin_struct(args)]` on an enum  → ValueEnum + serde + JsonSchema
//!     (CLI value choice; the author keeps any `#[derive(Default)]`/`Copy`)
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
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, Ident, Meta, Token, parse::Parse,
    parse::ParseStream, parse_quote, punctuated::Punctuated,
};

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

/// Map one `#[plugin(...)]` meta item to its `#[serde(...)]` equivalent token
/// stream. This is the whole point of the orca-native attribute namespace: a
/// plugin writes `#[plugin(skip_if_none)]` and never names serde. schemars
/// reads the emitted `#[serde(...)]` attributes directly, so a single
/// translation covers both (de)serialization and JSON-schema shape.
fn map_plugin_meta(m: &Meta) -> syn::Result<TokenStream2> {
    match m {
        // Bare flags: `#[plugin(skip)]`, `#[plugin(default)]`, …
        Meta::Path(p) => {
            let id = p
                .get_ident()
                .ok_or_else(|| syn::Error::new_spanned(p, "expected an identifier flag"))?
                .to_string();
            Ok(match id.as_str() {
                "skip" => quote! { skip },
                "default" => quote! { default },
                "flatten" => quote! { flatten },
                "untagged" => quote! { untagged },
                "transparent" => quote! { transparent },
                "deny_unknown" => quote! { deny_unknown_fields },
                // The single most common boilerplate: omit `None` from output.
                "skip_if_none" => quote! { skip_serializing_if = "Option::is_none" },
                other => {
                    return Err(syn::Error::new_spanned(
                        p,
                        format!("unknown #[plugin(...)] flag '{other}'"),
                    ));
                }
            })
        }
        // key = value: `#[plugin(rename_all = "camelCase")]`, …
        Meta::NameValue(nv) => {
            let key = nv
                .path
                .get_ident()
                .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected an identifier key"))?
                .to_string();
            let val = &nv.value;
            Ok(match key.as_str() {
                "rename_all" => quote! { rename_all = #val },
                "rename" => quote! { rename = #val },
                "tag" => quote! { tag = #val },
                "content" => quote! { content = #val },
                "alias" => quote! { alias = #val },
                "with" => quote! { with = #val },
                "default" => quote! { default = #val },
                // orca spelling → serde spelling.
                "skip_if" => quote! { skip_serializing_if = #val },
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!("unknown #[plugin(...)] key '{other}'"),
                    ));
                }
            })
        }
        Meta::List(l) => Err(syn::Error::new_spanned(
            l,
            "nested #[plugin(...)] lists are not supported",
        )),
    }
}

/// Rewrite every `#[plugin(...)]` attribute in `attrs` into the corresponding
/// `#[serde(...)]` attribute (dropping the `#[plugin(...)]` original). Applied
/// to the container, every field, and every enum variant so the plugin source
/// never mentions serde at any level.
fn translate_attrs(attrs: &mut Vec<Attribute>) -> syn::Result<()> {
    let mut serde_parts: Vec<TokenStream2> = Vec::new();
    for attr in attrs.iter() {
        if !attr.path().is_ident("plugin") {
            continue;
        }
        let metas = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        for m in &metas {
            serde_parts.push(map_plugin_meta(m)?);
        }
    }
    if serde_parts.is_empty() {
        return Ok(());
    }
    attrs.retain(|a| !a.path().is_ident("plugin"));
    let serde_attr: Attribute = parse_quote! { #[serde( #( #serde_parts ),* )] };
    attrs.push(serde_attr);
    Ok(())
}

/// Walk the whole item — container, fields, and (for enums) variants and their
/// fields — translating `#[plugin(...)]` → `#[serde(...)]` everywhere.
fn translate_item(item: &mut DeriveInput) -> syn::Result<()> {
    translate_attrs(&mut item.attrs)?;
    match &mut item.data {
        Data::Struct(s) => translate_fields(&mut s.fields)?,
        Data::Enum(e) => {
            for v in e.variants.iter_mut() {
                translate_attrs(&mut v.attrs)?;
                translate_fields(&mut v.fields)?;
            }
        }
        Data::Union(_) => {}
    }
    Ok(())
}

fn translate_fields(fields: &mut Fields) -> syn::Result<()> {
    match fields {
        Fields::Named(n) => {
            for f in n.named.iter_mut() {
                translate_attrs(&mut f.attrs)?;
            }
        }
        Fields::Unnamed(u) => {
            for f in u.unnamed.iter_mut() {
                translate_attrs(&mut f.attrs)?;
            }
        }
        Fields::Unit => {}
    }
    Ok(())
}

pub(crate) fn expand(attr: PluginStructAttr, mut item: DeriveInput) -> TokenStream2 {
    if let Err(e) = translate_item(&mut item) {
        return e.to_compile_error();
    }
    let crate_path = &attr.crate_path;
    // `#[serde(crate = "...")]` / `#[schemars(crate = "...")]` take a string
    // literal, not a path token. Stringify by rendering the path's tokens.
    let crate_path_str = crate_path.to_token_stream().to_string().replace(' ', "");
    let serde_path = format!("{crate_path_str}::serde");
    let schemars_path = format!("{crate_path_str}::schemars");

    // An `args` enum is a CLI value choice, not a flag group: it derives
    // `clap::ValueEnum`, where an `args` struct derives `clap::Args` + Default.
    // This lets one macro thin both shapes — a plugin's arg enums
    // (`EngineFlavor`, `Channel`, …) stop hand-writing the 8-line verbose
    // derive. `Default` is left to the author for enums (it needs a `#[default]`
    // variant, which not every value enum has).
    let is_enum = matches!(item.data, Data::Enum(_));
    let derives = if attr.args && is_enum {
        quote! {
            #[derive(
                #crate_path::clap::ValueEnum,
                #crate_path::serde::Serialize,
                #crate_path::serde::Deserialize,
                #crate_path::schemars::JsonSchema,
            )]
        }
    } else if attr.args {
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
