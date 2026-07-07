//! `#[plugin_error]` attribute macro.
//!
//! The orca-native error abstraction: a plugin declares an error enum without
//! ever naming `thiserror`. Each variant carries a `#[plugin(display = "…")]`
//! message (same `{field}` / `{0}` capture grammar as `format!`), and an
//! optional `#[plugin(from)]` flag on a single-field variant generates the
//! matching `From<Inner>` conversion. The macro emits `Display` +
//! `std::error::Error` (+ `From`) impls directly — no dependency on any
//! external error crate leaks into the plugin.
//!
//! ```rust,ignore
//! #[plugin_error]
//! pub enum SmbError {
//!     #[plugin(display = "required tool not found on PATH: {0}")]
//!     MissingTool(&'static str),
//!     #[plugin(display = "smb tool failed: {tool} (exit {code:?}): {stderr}")]
//!     ToolFailed { tool: &'static str, code: Option<i32>, stderr: String },
//!     #[plugin(display = "io: {0}", from)]
//!     Io(std::io::Error),
//!     #[plugin(display = "unsupported on this platform")]
//!     Unsupported,
//! }
//! ```

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Expr, ExprLit, Fields, Lit, Meta, Token, Type,
    punctuated::Punctuated,
};

pub(crate) fn expand(item: DeriveInput) -> TokenStream2 {
    match expand_inner(item) {
        Ok(ts) => ts,
        Err(e) => e.to_compile_error(),
    }
}

struct VariantPlugin {
    display: String,
    from: bool,
}

/// Pull `#[plugin(display = "...", from)]` off a variant, returning the parsed
/// config and stripping the `#[plugin(...)]` attribute from `attrs` so the
/// emitted enum is clean.
fn take_variant_plugin(
    attrs: &mut Vec<Attribute>,
    span: &syn::Ident,
) -> syn::Result<VariantPlugin> {
    let mut display: Option<String> = None;
    let mut from = false;
    for attr in attrs.iter() {
        if !attr.path().is_ident("plugin") {
            continue;
        }
        let metas = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        for m in &metas {
            match m {
                Meta::NameValue(nv) if nv.path.is_ident("display") => {
                    if let Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }) = &nv.value
                    {
                        display = Some(s.value());
                    } else {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "#[plugin(display = ...)] expects a string literal",
                        ));
                    }
                }
                Meta::Path(p) if p.is_ident("from") => from = true,
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "#[plugin_error] variants accept only `display = \"...\"` and `from`",
                    ));
                }
            }
        }
    }
    attrs.retain(|a| !a.path().is_ident("plugin"));
    let display = display.ok_or_else(|| {
        syn::Error::new_spanned(
            span,
            format!("variant `{span}` is missing #[plugin(display = \"...\")]"),
        )
    })?;
    Ok(VariantPlugin { display, from })
}

fn expand_inner(mut item: DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Enum(data) = &mut item.data else {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "#[plugin_error] can only be applied to an enum",
        ));
    };

    let mut display_arms: Vec<TokenStream2> = Vec::new();
    let mut from_impls: Vec<TokenStream2> = Vec::new();
    // `from` variants double as the error's `source()` (matching thiserror's
    // `#[from]`), so the std error chain is preserved without the plugin
    // naming any error crate.
    let mut source_arms: Vec<TokenStream2> = Vec::new();
    let enum_ident = item.ident.clone();

    for variant in data.variants.iter_mut() {
        let vident = variant.ident.clone();
        let cfg = take_variant_plugin(&mut variant.attrs, &vident)?;
        let display = &cfg.display;

        // Build the Display match arm + (optional) From impl per field shape.
        match &variant.fields {
            Fields::Unit => {
                display_arms.push(quote! {
                    Self::#vident => ::core::write!(f, #display),
                });
                if cfg.from {
                    return Err(syn::Error::new_spanned(
                        &vident,
                        "#[plugin(from)] needs a single-field variant",
                    ));
                }
            }
            Fields::Named(named) => {
                let binds: Vec<&syn::Ident> = named
                    .named
                    .iter()
                    .map(|f| f.ident.as_ref().expect("named field"))
                    .collect();
                display_arms.push(quote! {
                    Self::#vident { #( #binds ),* } => ::core::write!(f, #display),
                });
                if cfg.from {
                    if binds.len() != 1 {
                        return Err(syn::Error::new_spanned(
                            &vident,
                            "#[plugin(from)] needs exactly one field",
                        ));
                    }
                    let field = binds[0];
                    let ty = &named.named[0].ty;
                    from_impls.push(from_impl(
                        &enum_ident,
                        &item.generics,
                        quote! {
                            Self::#vident { #field: __v }
                        },
                        ty,
                    ));
                    source_arms.push(quote! {
                        Self::#vident { #field: __src, .. } => ::core::option::Option::Some(__src),
                    });
                }
            }
            Fields::Unnamed(unnamed) => {
                let n = unnamed.unnamed.len();
                let binds: Vec<syn::Ident> = (0..n)
                    .map(|i| syn::Ident::new(&format!("__{i}"), vident.span()))
                    .collect();
                display_arms.push(quote! {
                    Self::#vident( #( #binds ),* ) => ::core::write!(f, #display, #( #binds ),* ),
                });
                if cfg.from {
                    if n != 1 {
                        return Err(syn::Error::new_spanned(
                            &vident,
                            "#[plugin(from)] needs a single-field variant",
                        ));
                    }
                    let ty = &unnamed.unnamed[0].ty;
                    from_impls.push(from_impl(
                        &enum_ident,
                        &item.generics,
                        quote! { Self::#vident(__v) },
                        ty,
                    ));
                    source_arms.push(quote! {
                        Self::#vident(__src) => ::core::option::Option::Some(__src),
                    });
                }
            }
        }
    }

    ensure_debug_derive(&mut item.attrs);

    let (impl_g, ty_g, where_g) = item.generics.split_for_impl();

    // Only emit `source()` when at least one variant carries a source (a
    // `from` field); otherwise the default (returns `None`) is correct and a
    // match with only `_ => None` would be a dead-code wildcard.
    let source_fn = if source_arms.is_empty() {
        quote! {}
    } else {
        quote! {
            fn source(&self) -> ::core::option::Option<&(dyn ::std::error::Error + 'static)> {
                #[allow(unreachable_patterns)]
                match self {
                    #( #source_arms )*
                    _ => ::core::option::Option::None,
                }
            }
        }
    };

    let expanded = quote! {
        #item

        impl #impl_g ::core::fmt::Display for #enum_ident #ty_g #where_g {
            #[allow(unused_variables)]
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    #( #display_arms )*
                }
            }
        }

        impl #impl_g ::std::error::Error for #enum_ident #ty_g #where_g {
            #source_fn
        }

        #( #from_impls )*
    };
    Ok(expanded)
}

fn from_impl(
    enum_ident: &syn::Ident,
    generics: &syn::Generics,
    constructor: TokenStream2,
    ty: &Type,
) -> TokenStream2 {
    let (impl_g, ty_g, where_g) = generics.split_for_impl();
    quote! {
        impl #impl_g ::core::convert::From<#ty> for #enum_ident #ty_g #where_g {
            fn from(__v: #ty) -> Self {
                #constructor
            }
        }
    }
}

/// Inject `#[derive(Debug)]` unless the enum already derives it — `Error`
/// requires `Debug`, and the plugin author shouldn't have to remember it.
fn ensure_debug_derive(attrs: &mut Vec<Attribute>) {
    let has_debug = attrs.iter().any(|a| {
        a.path().is_ident("derive")
            && a.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .map(|metas| metas.iter().any(|m| m.path().is_ident("Debug")))
                .unwrap_or(false)
    });
    if !has_debug {
        attrs.push(syn::parse_quote! { #[derive(Debug)] });
    }
}
