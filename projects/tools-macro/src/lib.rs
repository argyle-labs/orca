//! `#[orca_tool]` proc-macro — proof-of-shape entry point.
//!
//! Annotate an async function with the standard tool signature and the macro
//! emits the four-surface scaffolding inline next to the body:
//!
//! ```rust,ignore
//! #[orca_tool(domain = "host", verb = "info")]
//! /// Doc comment becomes OrcaToolDef::DESCRIPTION.
//! async fn host_info(args: EmptyArgs, ctx: &ToolCtx) -> Result<HostInfoOutput> { /* … */ }
//! ```
//!
//! Emits (in the same crate as the function):
//!   - `pub struct HostInfo;` (ZST keyed off the camelcased fn name)
//!   - `impl OrcaToolDef for HostInfo` — NAME = fn ident, DESCRIPTION = doc.
//!   - `#[async_trait] impl OrcaTool for HostInfo` — thunk that calls the
//!     annotated fn.
//!   - `impl OrcaOp for HostInfo` (always — every annotated tool participates
//!     in the unified domain/verb namespace).
//!   - `#[cfg(feature = "native")] inventory::submit!` into the
//!     `ORCA_TOOLS` slice exposed by `orca-tools-def` so the registry picks
//!     it up at startup without any central enrollment list.
//!   - `#[cfg(feature = "wasm")] #[wasm_bindgen] impl OrcaClient { … }` — one
//!     typed JS method per tool. Lives in the same crate as the fn, which is
//!     fine because all current tool bodies sit in `orca-tools-def` where
//!     `OrcaClient` is defined. Cross-crate emission is a follow-up.
//!
//! Scope: this slice only supports the canonical `async fn name(args: T,
//! ctx: &ToolCtx) -> Result<O>` form. Named-parameter expansion can be added
//! later by destructuring `args` inside the thunk.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{
    Attribute, Expr, ExprLit, FnArg, Ident, ItemFn, Lit, LitStr, Meta, MetaNameValue, Pat, PatType,
    ReturnType, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
};

/// Parsed contents of `#[orca_tool(domain = "...", verb = "...", cli = ident)]`.
struct ToolAttr {
    domain: LitStr,
    verb: LitStr,
    cli_mode: Option<Ident>,
}

impl Parse for ToolAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let items = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut domain = None;
        let mut verb = None;
        let mut cli_mode = None;
        for nv in items {
            let key = nv
                .path
                .get_ident()
                .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected ident"))?
                .to_string();
            match key.as_str() {
                "domain" => domain = Some(lit_str(&nv.value)?),
                "verb" => verb = Some(lit_str(&nv.value)?),
                "cli" => {
                    // accept either an ident (cli = manual) or a string ("manual")
                    cli_mode = Some(match &nv.value {
                        Expr::Path(p) => p
                            .path
                            .get_ident()
                            .ok_or_else(|| syn::Error::new_spanned(&nv.value, "expected ident"))?
                            .clone(),
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }) => Ident::new(&s.value(), s.span()),
                        _ => return Err(syn::Error::new_spanned(&nv.value, "expected ident")),
                    });
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!("unknown key: {other}"),
                    ));
                }
            }
        }
        Ok(Self {
            domain: domain
                .ok_or_else(|| syn::Error::new(Span::call_site(), "missing `domain = \"…\"`"))?,
            verb: verb
                .ok_or_else(|| syn::Error::new(Span::call_site(), "missing `verb = \"…\"`"))?,
            cli_mode,
        })
    }
}

fn lit_str(expr: &Expr) -> syn::Result<LitStr> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Ok(s.clone()),
        _ => Err(syn::Error::new_spanned(expr, "expected string literal")),
    }
}

#[proc_macro_attribute]
pub fn orca_tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as ToolAttr);
    let item = parse_macro_input!(item as ItemFn);

    match expand(attr, item) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(attr: ToolAttr, item: ItemFn) -> syn::Result<TokenStream2> {
    if item.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            item.sig.fn_token,
            "`#[orca_tool]` requires `async fn`",
        ));
    }
    let fn_ident = item.sig.ident.clone();
    let fn_name_str = fn_ident.to_string();
    let zst_ident = Ident::new(&snake_to_pascal(&fn_name_str), fn_ident.span());

    // Parse `(args: ArgsTy, ctx: &ToolCtx)`. We accept underscored names too.
    let mut sig_iter = item.sig.inputs.iter();
    let (args_pat, args_ty) = match sig_iter.next() {
        Some(FnArg::Typed(PatType { pat, ty, .. })) => (pat.clone(), (**ty).clone()),
        _ => {
            return Err(syn::Error::new_spanned(
                &item.sig.inputs,
                "expected first param `args: ArgsTy`",
            ));
        }
    };
    let _ctx_arg = match sig_iter.next() {
        Some(FnArg::Typed(PatType { pat, ty, .. })) => Some((pat.clone(), (**ty).clone())),
        None => None,
        _ => {
            return Err(syn::Error::new_spanned(
                &item.sig.inputs,
                "second param must be `ctx: &ToolCtx`",
            ));
        }
    };

    // Return type: `Result<OutputTy>` or `Result<OutputTy, ErrTy>` — we only
    // care about OutputTy for the OrcaToolDef::Output projection.
    let output_ty = extract_ok_ty(&item.sig.output).ok_or_else(|| {
        syn::Error::new_spanned(
            &item.sig.output,
            "expected `-> Result<OutputTy>` or `-> Result<OutputTy, _>`",
        )
    })?;

    let description = collect_doc(&item.attrs).unwrap_or_else(|| fn_name_str.clone());

    let domain = attr.domain;
    let verb = attr.verb;
    let tool_name = format!("{}.{}", domain.value(), verb.value());

    // Decide whether to render an args binding `let args = ...` (real ident)
    // or just discard (underscored).
    let needs_args_binding = match &*args_pat {
        Pat::Ident(p) => !p.ident.to_string().starts_with('_'),
        _ => true,
    };
    let args_param = if needs_args_binding {
        quote! { #args_pat: #args_ty }
    } else {
        quote! { _args: #args_ty }
    };
    let args_forward = if needs_args_binding {
        // The annotated fn keeps using its original parameter name; we just
        // forward by re-binding to that name.
        match &*args_pat {
            Pat::Ident(p) => {
                let id = &p.ident;
                quote! { #id }
            }
            _ => quote! { __orca_args },
        }
    } else {
        quote! { _args }
    };

    let ctx_param_name = Ident::new("ctx", Span::call_site());
    let ctx_param = quote! { #ctx_param_name: &::orca_utils::tool::ToolCtx };

    // Doc string keeps the original — we just relocate the description into
    // the const.
    let inner_fn = item;

    // CLI behaviour: default emits register_op!; `manual`/`skip` mirror the
    // existing semantics.
    let cli_block = match attr.cli_mode.as_ref().map(|i| i.to_string()).as_deref() {
        Some("manual") | Some("skip") => quote! {},
        _ => quote! {
            #[cfg(feature = "cli")]
            const _: () = {
                ::orca_tools_def::register_op! {
                    tool: #zst_ident,
                    domain: #domain,
                    verb: #verb,
                    summary: <#zst_ident as ::orca_tools_def::OrcaToolDef>::DESCRIPTION,
                }
            };
        },
    };

    // WASM method emission — gated. Works as long as `#[orca_tool]` is used
    // in the same crate as `OrcaClient` (orca-tools-def today). Cross-crate
    // emission is a follow-up.
    let wasm_block = quote! {
        #[cfg(feature = "wasm")]
        const _: () = {
            use ::orca_tools_def::wasm::OrcaClient;
            use ::orca_tools_def::OrcaToolDef;
            use ::wasm_bindgen::prelude::*;
            #[wasm_bindgen]
            impl OrcaClient {
                #[wasm_bindgen]
                pub async fn #fn_ident(
                    &self,
                    args: <#zst_ident as OrcaToolDef>::Args,
                ) -> Result<<#zst_ident as OrcaToolDef>::Output, ::wasm_bindgen::JsValue> {
                    self.call_tool_typed::<#zst_ident>(args).await
                }
            }
        };
    };

    // Wrap the user-authored fn in `#[cfg(feature = "native")]` — its body
    // is what brings in the native deps (db, integrations, etc.). The
    // OrcaToolDef + wasm method emissions stay unconditional so wasm-only
    // builds keep their typed OrcaClient methods.
    let expanded = quote! {
        #[cfg(feature = "native")]
        #inner_fn

        #[allow(non_camel_case_types)]
        pub struct #zst_ident;

        impl ::orca_tools_def::OrcaToolDef for #zst_ident {
            const NAME: &'static str = #tool_name;
            const DESCRIPTION: &'static str = #description;
            type Args = #args_ty;
            type Output = #output_ty;
        }

        impl ::orca_tools_def::OrcaOp for #zst_ident {
            const DOMAIN: &'static str = #domain;
            const VERB: &'static str = #verb;
        }

        #[cfg(feature = "native")]
        #[::async_trait::async_trait]
        impl ::orca_utils::tool::OrcaTool for #zst_ident {
            async fn run(
                #args_param,
                #ctx_param,
            ) -> ::anyhow::Result<#output_ty> {
                #fn_ident(#args_forward, #ctx_param_name).await
            }
        }

        #[cfg(feature = "native")]
        ::inventory::submit! {
            ::orca_tools_def::ToolRegistration {
                name: #tool_name,
                register: |reg| {
                    reg.register::<#zst_ident>();
                },
            }
        }

        #cli_block
        #wasm_block
    };

    Ok(expanded)
}

fn extract_ok_ty(ret: &ReturnType) -> Option<Type> {
    let ty = match ret {
        ReturnType::Type(_, t) => t,
        _ => return None,
    };
    // Match `Result<T>` or `Result<T, E>` — we accept any path ending in `Result`.
    let path = match &**ty {
        Type::Path(tp) => &tp.path,
        _ => return None,
    };
    let last = path.segments.last()?;
    if last.ident != "Result" {
        return None;
    }
    let args = match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => a,
        _ => return None,
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

fn collect_doc(attrs: &[Attribute]) -> Option<String> {
    let mut out = String::new();
    for a in attrs {
        if !a.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(MetaNameValue {
            value: Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }),
            ..
        }) = &a.meta
        {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s.value().trim());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn snake_to_pascal(s: &str) -> String {
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
