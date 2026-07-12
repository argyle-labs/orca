//! `#[endpoint_tool]` — orca's sugar for the overwhelmingly common tool shape:
//! "resolve a registered endpoint's client, call it, return the result."
//!
//! An API plugin's tools nearly all repeat the same scaffolding: an args struct
//! with the full derive set, an `endpoint: String` field, an 8-line `make_client`
//! resolve-and-enable-check, and an `#[orca_tool]` wrapper. `#[endpoint_tool]`
//! collapses all of it — the author writes only the endpoint call:
//!
//! ```rust,ignore
//! #[endpoint_tool(domain = "home-assistant", verb = "entities")]
//! /// List entities (optionally domain-filtered).
//! async fn ha_entities(client: Client, #[arg(long)] domain: Option<String>) -> Result<JsonAny> {
//!     Ok(client.entity_list(domain.as_deref()).await?.into())
//! }
//! ```
//!
//! The first parameter is the resolved client (by convention produced by an
//! in-scope `make_client(&str) -> Result<Client>`, overridable with
//! `resolve = <path>`). Every other parameter — carrying its own `#[arg(...)]` /
//! `#[serde(...)]` attributes — becomes a field on a generated `<Name>Args`
//! struct alongside the always-present `endpoint`. The macro emits that struct
//! (via `#[plugin_struct]`) plus an `#[orca_tool]` wrapper that resolves the
//! client, binds the args, and runs the body. Any attribute args other than
//! `resolve` (e.g. `domain`, `verb`, `role`) pass straight through to
//! `#[orca_tool]`.
//!
//! Tools that need `&ToolCtx`, multiple clients, or no endpoint stay plain
//! `#[orca_tool]` — this is the sugar for the common case, not a replacement.

use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{FnArg, ItemFn, Meta, Path, Token, parse_quote, parse2};

pub fn expand(attr: TokenStream2, item: TokenStream2) -> TokenStream2 {
    let func: ItemFn = match parse2(item) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error(),
    };
    let metas = match Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error(),
    };

    // Split `resolve = <path>` (the client resolver, default `make_client`) out
    // of the attribute; everything else forwards verbatim to `#[orca_tool]`.
    let mut resolve: Path = parse_quote!(make_client);
    let mut tool_metas: Vec<Meta> = Vec::new();
    for m in metas {
        if let Meta::NameValue(nv) = &m
            && nv.path.is_ident("resolve")
        {
            match syn::parse2::<Path>(nv.value.to_token_stream()) {
                Ok(p) => resolve = p,
                Err(e) => return e.to_compile_error(),
            }
            continue;
        }
        tool_metas.push(m);
    }

    // First param = the resolved client; the rest become args-struct fields.
    let mut inputs = func.sig.inputs.iter();
    let client_arg = match inputs.next() {
        Some(FnArg::Typed(pt)) => pt,
        _ => {
            return syn::Error::new_spanned(
                &func.sig,
                "#[endpoint_tool] fn must take the resolved client as its first parameter",
            )
            .to_compile_error();
        }
    };
    let client_pat = &client_arg.pat;

    let mut fields = Vec::new();
    let mut binds = Vec::new();
    for arg in inputs {
        let FnArg::Typed(pt) = arg else {
            return syn::Error::new_spanned(arg, "#[endpoint_tool] does not take a receiver")
                .to_compile_error();
        };
        let attrs = &pt.attrs; // forwarded #[arg(...)]/#[serde(...)] on the param
        let pat = &pt.pat;
        let ty = &pt.ty;
        fields.push(quote! { #(#attrs)* pub #pat: #ty });
        binds.push(quote! { let #pat = args.#pat; });
    }

    let fn_name = &func.sig.ident;
    let struct_name = format_ident!("{}Args", to_pascal(&fn_name.to_string()));
    let ret = &func.sig.output;
    let body = &func.block;
    // Preserve doc comments (and any other outer attrs) on the tool fn.
    let docs = func
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .collect::<Vec<_>>();

    quote! {
        #[plugin_struct(args)]
        pub struct #struct_name {
            /// Registered endpoint name.
            #[arg(long)]
            pub endpoint: String,
            #(#fields,)*
        }

        #[orca_tool(#(#tool_metas),*)]
        #(#docs)*
        async fn #fn_name(args: #struct_name, _ctx: &ToolCtx) #ret {
            let #client_pat = #resolve(&args.endpoint)?;
            #(#binds)*
            #body
        }
    }
}

/// `ha_entities` → `HaEntities`. Mirrors the tool-name casing used elsewhere.
fn to_pascal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if c == '_' {
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
