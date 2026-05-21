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
//!   - An `OpenApiToolRegistration` inventory entry — the spec endpoint hoists
//!     every tool path automatically (see `tools-def::openapi`).
//!
//! Scope: this slice only supports the canonical `async fn name(args: T,
//! ctx: &ToolCtx) -> Result<O>` form. Named-parameter expansion can be added
//! later by destructuring `args` inside the thunk.

#[cfg(not(test))]
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
#[cfg(not(test))]
use syn::parse_macro_input;
use syn::{
    Attribute, Expr, ExprLit, FnArg, Ident, ItemFn, Lit, LitStr, Meta, MetaNameValue, Pat, PatType,
    ReturnType, Token, Type,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

/// Parsed contents of `#[orca_tool(domain = "...", verb = "...", cli = ident)]`.
struct ToolAttr {
    domain: LitStr,
    verb: LitStr,
    cli_mode: Option<Ident>,
    /// Opt-in: `#[orca_tool(..., remote_ok = true)]` makes this tool callable
    /// by paired pod peers via `pod/exec`. Default false.
    remote_ok: bool,
    /// Minimum role required to invoke this tool via authenticated surfaces.
    /// `"any"` (default) means any authenticated identity passes; `"admin"`
    /// requires `AuthIdentity::role == "admin"`. Set via
    /// `#[orca_tool(..., role = "admin")]`.
    role: Option<LitStr>,
}

impl Parse for ToolAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let items = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut domain = None;
        let mut verb = None;
        let mut cli_mode = None;
        let mut remote_ok = false;
        let mut role: Option<LitStr> = None;
        for nv in items {
            let key = nv
                .path
                .get_ident()
                .ok_or_else(|| syn::Error::new_spanned(&nv.path, "expected ident"))?
                .to_string();
            match key.as_str() {
                "domain" => domain = Some(lit_str(&nv.value)?),
                "verb" => verb = Some(lit_str(&nv.value)?),
                "remote_ok" => {
                    remote_ok = match &nv.value {
                        Expr::Lit(ExprLit {
                            lit: Lit::Bool(b), ..
                        }) => b.value,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "remote_ok expects a bool literal",
                            ));
                        }
                    };
                }
                "role" => {
                    let s = lit_str(&nv.value)?;
                    match s.value().as_str() {
                        "any" | "admin" => {}
                        other => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                format!("role must be \"any\" or \"admin\", got {other:?}"),
                            ));
                        }
                    }
                    role = Some(s);
                }
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
            remote_ok,
            role,
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

// The proc_macro_attribute entry is a thin trampoline into `expand_to_tokens`
// — it parses TokenStreams that only exist during downstream compilation, so
// unit tests can't drive it. Gating it on `not(test)` keeps it instrumented
// by the production build (where it's the only public surface) and out of
// the test build's coverage denominator. Tests cover `expand_to_tokens`
// directly.
#[cfg(not(test))]
#[proc_macro_attribute]
pub fn orca_tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as ToolAttr);
    let item = parse_macro_input!(item as ItemFn);
    expand_to_tokens(attr, item).into()
}

/// Wrapper around `expand` that flattens `Result` into a single `TokenStream2`,
/// turning errors into compile_error invocations. Pulled out so the
/// error-flattening branch is testable — the `#[proc_macro_attribute]` entry
/// above is unreachable from unit tests.
fn expand_to_tokens(attr: ToolAttr, item: ItemFn) -> TokenStream2 {
    match expand(attr, item) {
        Ok(ts) => ts,
        Err(e) => e.to_compile_error(),
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
    // Peek the second arg — we don't use its type (the thunk hardcodes
    // `&ToolCtx`) but require that, if present, it's a typed positional
    // param rather than a `self` receiver. Iteration after the first param
    // is guaranteed by Rust's grammar to be either Typed or absent, so
    // there is no `_` arm to defend against.
    let _ = sig_iter.next();

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
    let remote_ok_lit = attr.remote_ok;
    // REQUIRED_ROLE: explicit `role = "..."` wins; otherwise default-deny
    // derives from the verb — read-shaped verbs (`list`/`detail`/`search`) get
    // "any", anything else gets "admin". This closes C2 (default-deny on
    // mutating endpoints) — see `feedback_crud_unification_blocks_security`.
    let role_const = match attr.role.as_ref() {
        Some(s) => quote! { const REQUIRED_ROLE: &'static str = #s; },
        None => {
            let derived = match verb.value().as_str() {
                "list" | "detail" | "search" => "any",
                _ => "admin",
            };
            quote! { const REQUIRED_ROLE: &'static str = #derived; }
        }
    };

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

    // OpenAPI emission — unconditional (the spec is built from schemars, no
    // native deps needed). Every tool gets one `/api/tools/<NAME>` POST entry
    // injected into the spec at runtime.
    let openapi_block = quote! {
        ::inventory::submit! {
            ::orca_tools_def::openapi::OpenApiToolRegistration {
                name: #tool_name,
                description: #description,
                domain: #domain,
                args_schema: || {
                    ::serde_json::to_value(
                        ::schemars::schema_for!(<#zst_ident as ::orca_tools_def::OrcaToolDef>::Args)
                    ).unwrap_or(::serde_json::Value::Object(::serde_json::Map::new()))
                },
                output_schema: || {
                    ::serde_json::to_value(
                        ::schemars::schema_for!(<#zst_ident as ::orca_tools_def::OrcaToolDef>::Output)
                    ).unwrap_or(::serde_json::Value::Object(::serde_json::Map::new()))
                },
            }
        }
    };

    // Wrap the user-authored fn in `#[cfg(feature = "native")]` — its body
    // is what brings in the native deps (db, integrations, etc.). The
    // OrcaToolDef emission stays unconditional.
    let expanded = quote! {
        #[cfg(feature = "native")]
        #inner_fn

        #[allow(non_camel_case_types)]
        pub struct #zst_ident;

        impl ::orca_tools_def::OrcaToolDef for #zst_ident {
            const NAME: &'static str = #tool_name;
            const DESCRIPTION: &'static str = #description;
            const REMOTE_OK: bool = #remote_ok_lit;
            #role_const
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
        #openapi_block
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

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse_quote;

    // ── snake_to_pascal ───────────────────────────────────────────────────────

    #[test]
    fn snake_to_pascal_capitalizes_single_word() {
        assert_eq!(snake_to_pascal("host"), "Host");
    }

    #[test]
    fn snake_to_pascal_handles_multiple_segments() {
        assert_eq!(snake_to_pascal("host_info_v2"), "HostInfoV2");
    }

    #[test]
    fn snake_to_pascal_handles_empty_string() {
        assert_eq!(snake_to_pascal(""), "");
    }

    #[test]
    fn snake_to_pascal_handles_leading_and_trailing_underscores() {
        // Leading/trailing underscores trigger the `cap = true` branch
        // without consuming a char — exercises both the `if c == '_'` and
        // `else if cap` branches.
        assert_eq!(snake_to_pascal("_foo_"), "Foo");
    }

    // ── collect_doc ───────────────────────────────────────────────────────────

    #[test]
    fn collect_doc_returns_none_for_no_doc_attrs() {
        let attrs: Vec<Attribute> = vec![parse_quote!(#[derive(Debug)])];
        assert!(collect_doc(&attrs).is_none());
    }

    #[test]
    fn collect_doc_collects_single_doc_line() {
        let attrs: Vec<Attribute> = vec![parse_quote!(#[doc = "hello"])];
        assert_eq!(collect_doc(&attrs).as_deref(), Some("hello"));
    }

    #[test]
    fn collect_doc_concatenates_multiple_lines_with_space() {
        let attrs: Vec<Attribute> = vec![
            parse_quote!(#[doc = "first line"]),
            parse_quote!(#[doc = "second line"]),
        ];
        assert_eq!(
            collect_doc(&attrs).as_deref(),
            Some("first line second line")
        );
    }

    #[test]
    fn collect_doc_ignores_non_doc_attrs() {
        let attrs: Vec<Attribute> = vec![
            parse_quote!(#[derive(Debug)]),
            parse_quote!(#[doc = "kept"]),
            parse_quote!(#[allow(dead_code)]),
        ];
        assert_eq!(collect_doc(&attrs).as_deref(), Some("kept"));
    }

    // ── extract_ok_ty ─────────────────────────────────────────────────────────

    #[test]
    fn extract_ok_ty_handles_result_single_arg() {
        let ret: ReturnType = parse_quote!(-> Result<u32>);
        let ty = extract_ok_ty(&ret).expect("ok type extracted");
        assert_eq!(quote!(#ty).to_string(), "u32");
    }

    #[test]
    fn extract_ok_ty_handles_result_two_args() {
        let ret: ReturnType = parse_quote!(-> Result<String, MyErr>);
        let ty = extract_ok_ty(&ret).unwrap();
        assert_eq!(quote!(#ty).to_string(), "String");
    }

    #[test]
    fn extract_ok_ty_returns_none_for_unit_return() {
        let ret: ReturnType = ReturnType::Default;
        assert!(extract_ok_ty(&ret).is_none());
    }

    #[test]
    fn extract_ok_ty_returns_none_for_non_result_path() {
        let ret: ReturnType = parse_quote!(-> Option<u32>);
        assert!(extract_ok_ty(&ret).is_none());
    }

    #[test]
    fn extract_ok_ty_returns_none_for_non_path_type() {
        // Tuple type — not a Type::Path.
        let ret: ReturnType = parse_quote!(-> (u32, u32));
        assert!(extract_ok_ty(&ret).is_none());
    }

    #[test]
    fn extract_ok_ty_returns_none_for_path_without_angle_brackets() {
        let ret: ReturnType = parse_quote!(-> Result);
        assert!(extract_ok_ty(&ret).is_none());
    }

    // ── lit_str ───────────────────────────────────────────────────────────────

    #[test]
    fn lit_str_accepts_string_literal() {
        let expr: Expr = parse_quote!("hello");
        assert_eq!(lit_str(&expr).unwrap().value(), "hello");
    }

    #[test]
    fn lit_str_rejects_non_string_literal() {
        let expr: Expr = parse_quote!(42);
        assert!(lit_str(&expr).is_err());
    }

    // ── ToolAttr parsing ──────────────────────────────────────────────────────

    fn parse_attr(ts: proc_macro2::TokenStream) -> syn::Result<ToolAttr> {
        syn::parse2(ts)
    }

    #[test]
    fn tool_attr_parses_minimum_required_fields() {
        let attr = parse_attr(quote!(domain = "host", verb = "info")).unwrap();
        assert_eq!(attr.domain.value(), "host");
        assert_eq!(attr.verb.value(), "info");
        assert!(!attr.remote_ok);
        assert!(attr.cli_mode.is_none());
        assert!(attr.role.is_none());
    }

    #[test]
    fn tool_attr_parses_all_optional_fields() {
        let attr = parse_attr(quote!(
            domain = "x",
            verb = "y",
            remote_ok = true,
            cli = manual,
            role = "admin"
        ))
        .unwrap();
        assert!(attr.remote_ok);
        assert_eq!(attr.cli_mode.unwrap().to_string(), "manual");
        assert_eq!(attr.role.unwrap().value(), "admin");
    }

    #[test]
    fn tool_attr_accepts_cli_as_string_literal() {
        let attr = parse_attr(quote!(domain = "x", verb = "y", cli = "skip")).unwrap();
        assert_eq!(attr.cli_mode.unwrap().to_string(), "skip");
    }

    #[test]
    fn tool_attr_rejects_non_bool_remote_ok() {
        let err = parse_attr(quote!(domain = "x", verb = "y", remote_ok = "true"))
            .err()
            .expect("expected parse error");
        assert!(err.to_string().contains("remote_ok"));
    }

    #[test]
    fn tool_attr_rejects_invalid_role_value() {
        let err = parse_attr(quote!(domain = "x", verb = "y", role = "wizard"))
            .err()
            .expect("expected parse error");
        assert!(err.to_string().contains("role must be"));
    }

    #[test]
    fn tool_attr_rejects_unknown_key() {
        let err = parse_attr(quote!(domain = "x", verb = "y", banana = "split"))
            .err()
            .expect("expected parse error");
        assert!(err.to_string().contains("unknown key"));
    }

    #[test]
    fn tool_attr_rejects_missing_domain() {
        let err = parse_attr(quote!(verb = "y"))
            .err()
            .expect("expected parse error");
        assert!(err.to_string().contains("missing `domain"));
    }

    #[test]
    fn tool_attr_rejects_missing_verb() {
        let err = parse_attr(quote!(domain = "x"))
            .err()
            .expect("expected parse error");
        assert!(err.to_string().contains("missing `verb"));
    }

    #[test]
    fn tool_attr_rejects_non_ident_cli_mode_expr() {
        // `cli = 42` — not an ident, not a string.
        let err = parse_attr(quote!(domain = "x", verb = "y", cli = 42))
            .err()
            .expect("expected parse error");
        assert!(err.to_string().contains("expected ident"));
    }

    // ── expand ────────────────────────────────────────────────────────────────

    fn ok_fn() -> ItemFn {
        parse_quote! {
            /// Doc line one.
            /// Doc line two.
            async fn host_info(args: HostInfoArgs, ctx: &ToolCtx) -> anyhow::Result<HostInfoOutput> {
                let _ = args;
                let _ = ctx;
                Ok(HostInfoOutput {})
            }
        }
    }

    fn attr_ok() -> ToolAttr {
        parse_attr(quote!(domain = "host", verb = "info")).unwrap()
    }

    #[test]
    fn expand_emits_zst_and_orca_tool_def_for_minimal_input() {
        let out = expand(attr_ok(), ok_fn()).unwrap().to_string();
        assert!(out.contains("pub struct HostInfo"), "got: {out}");
        assert!(out.contains("OrcaToolDef"), "got: {out}");
        assert!(out.contains("\"host.info\""), "got: {out}");
        // Doc lines collapsed with space separator.
        assert!(out.contains("Doc line one. Doc line two."), "got: {out}");
    }

    #[test]
    fn expand_with_remote_ok_emits_const_remote_ok_true() {
        let attr = parse_attr(quote!(domain = "h", verb = "v", remote_ok = true)).unwrap();
        let out = expand(attr, ok_fn()).unwrap().to_string();
        assert!(out.contains("REMOTE_OK : bool = true"), "got: {out}");
    }

    #[test]
    fn expand_with_role_emits_required_role_override() {
        let attr = parse_attr(quote!(domain = "h", verb = "v", role = "admin")).unwrap();
        let out = expand(attr, ok_fn()).unwrap().to_string();
        assert!(
            out.contains("REQUIRED_ROLE : & 'static str = \"admin\""),
            "got: {out}"
        );
    }

    #[test]
    fn expand_without_role_derives_required_role_from_verb() {
        // attr_ok = (verb="info") → not a read verb → defaults to "admin".
        let out = expand(attr_ok(), ok_fn()).unwrap().to_string();
        assert!(
            out.contains("REQUIRED_ROLE : & 'static str = \"admin\""),
            "got: {out}"
        );
    }

    #[test]
    fn expand_without_role_derives_any_for_read_verbs() {
        for verb in ["list", "detail", "search"] {
            let attr = parse_attr(quote!(domain = "h", verb = #verb)).unwrap();
            let out = expand(attr, ok_fn()).unwrap().to_string();
            assert!(
                out.contains("REQUIRED_ROLE : & 'static str = \"any\""),
                "verb={verb} got: {out}"
            );
        }
    }

    #[test]
    fn expand_cli_manual_skips_register_op_block() {
        let attr = parse_attr(quote!(domain = "h", verb = "v", cli = manual)).unwrap();
        let out = expand(attr, ok_fn()).unwrap().to_string();
        assert!(!out.contains("register_op"), "got: {out}");
    }

    #[test]
    fn expand_cli_skip_also_skips_register_op_block() {
        let attr = parse_attr(quote!(domain = "h", verb = "v", cli = skip)).unwrap();
        let out = expand(attr, ok_fn()).unwrap().to_string();
        assert!(!out.contains("register_op"), "got: {out}");
    }

    #[test]
    fn expand_default_cli_emits_register_op_block() {
        let out = expand(attr_ok(), ok_fn()).unwrap().to_string();
        assert!(out.contains("register_op"), "got: {out}");
    }

    #[test]
    fn expand_rejects_non_async_fn() {
        let item: ItemFn = parse_quote! {
            fn host_info(args: A, ctx: &ToolCtx) -> anyhow::Result<O> { unimplemented!() }
        };
        let err = expand(attr_ok(), item).expect_err("expected parse error");
        assert!(err.to_string().contains("async fn"));
    }

    #[test]
    fn expand_rejects_fn_with_no_args() {
        let item: ItemFn = parse_quote! {
            async fn host_info() -> anyhow::Result<O> { unimplemented!() }
        };
        let err = expand(attr_ok(), item).expect_err("expected parse error");
        assert!(err.to_string().contains("expected first param"));
    }

    #[test]
    fn expand_rejects_unparseable_return_type() {
        let item: ItemFn = parse_quote! {
            async fn host_info(args: A, ctx: &ToolCtx) -> Option<O> { unimplemented!() }
        };
        let err = expand(attr_ok(), item).expect_err("expected parse error");
        assert!(err.to_string().contains("Result"));
    }

    #[test]
    fn expand_underscored_args_param_uses_discarded_binding() {
        let item: ItemFn = parse_quote! {
            async fn host_info(_args: A, ctx: &ToolCtx) -> anyhow::Result<O> {
                let _ = ctx;
                unimplemented!()
            }
        };
        let out = expand(attr_ok(), item).unwrap().to_string();
        // The thunk should declare a `_args` param rather than re-binding.
        assert!(out.contains("_args"), "got: {out}");
    }

    #[test]
    fn expand_with_only_one_arg_treats_missing_ctx_as_none_branch() {
        // Single-arg fn — second param is absent, exercising `None => None`
        // in the ctx_arg match. Must still have a valid Result return.
        let item: ItemFn = parse_quote! {
            async fn host_info(args: A) -> anyhow::Result<O> {
                let _ = args;
                unimplemented!()
            }
        };
        // Expansion succeeds — ctx is synthesized into the thunk regardless.
        let out = expand(attr_ok(), item).unwrap().to_string();
        assert!(out.contains("HostInfo"));
    }

    #[test]
    fn expand_to_tokens_ok_returns_expansion() {
        let ts = expand_to_tokens(attr_ok(), ok_fn()).to_string();
        assert!(ts.contains("HostInfo"));
    }

    #[test]
    fn expand_to_tokens_err_returns_compile_error() {
        // Non-async fn → expand errors → expand_to_tokens flattens into a
        // compile_error invocation.
        let item: ItemFn = parse_quote! {
            fn host_info(args: A, ctx: &ToolCtx) -> anyhow::Result<O> { unimplemented!() }
        };
        let ts = expand_to_tokens(attr_ok(), item).to_string();
        assert!(ts.contains("compile_error"), "got: {ts}");
    }

    #[test]
    fn expand_handles_non_ident_args_pattern() {
        // Tuple-destructured args param: `(a, b): (u32, u32)` — Pat is not
        // Pat::Ident, exercising the `_ => true` branch in `needs_args_binding`
        // AND the `_ => quote!(__orca_args)` branch in `args_forward`.
        let item: ItemFn = parse_quote! {
            async fn host_info((a, b): (u32, u32), ctx: &ToolCtx) -> anyhow::Result<O> {
                let _ = (a, b, ctx);
                unimplemented!()
            }
        };
        let out = expand(attr_ok(), item).unwrap().to_string();
        assert!(out.contains("__orca_args"), "got: {out}");
    }

    #[test]
    fn extract_ok_ty_skips_non_type_generic_args() {
        // Result<'a, T> — the first generic argument is a lifetime, not a
        // type. `find_map` should skip it and pick T.
        let ret: ReturnType = parse_quote!(-> Result<'a, T>);
        let ty = extract_ok_ty(&ret).expect("ok type extracted");
        assert_eq!(quote!(#ty).to_string(), "T");
    }

    #[test]
    fn collect_doc_ignores_non_namevalue_doc_attrs() {
        // `#[doc(hidden)]` is Meta::List, not Meta::NameValue — the inner
        // `if let` falls through without appending to `out`, so a sole
        // doc(hidden) attr yields None (out is empty).
        let attrs: Vec<Attribute> = vec![parse_quote!(#[doc(hidden)])];
        assert!(collect_doc(&attrs).is_none());
    }

    #[test]
    fn expand_no_doc_falls_back_to_fn_name_as_description() {
        let item: ItemFn = parse_quote! {
            async fn host_info(args: A, ctx: &ToolCtx) -> anyhow::Result<O> {
                let _ = (args, ctx);
                unimplemented!()
            }
        };
        let out = expand(attr_ok(), item).unwrap().to_string();
        assert!(out.contains("\"host_info\""), "got: {out}");
    }
}
