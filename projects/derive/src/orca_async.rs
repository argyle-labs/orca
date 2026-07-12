//! `#[orca_async]` — orca's native sugar for async traits.
//!
//! This is *the* way to write an async trait or impl in orca: annotate it and
//! write plain `async fn` methods. A plugin author (or core) never touches
//! `Box::pin`, future pinning, lifetime bookkeeping, or a runtime — orca owns
//! all of that behind this one attribute. It works on `dyn`-dispatched traits
//! (`Arc<dyn StorageBackend>`, the whole plug-in registry model), which native
//! `async fn`-in-trait still can't express.
//!
//! ```rust,ignore
//! #[orca_async]
//! impl StorageBackend for MyBackend {
//!     async fn mount(&self, id: &str) -> Result<MountOutcome> { /* just await */ }
//! }
//! ```
//!
//! Scope: `&self` / `&mut self` / by-value `self` receivers, reference and value
//! parameters (elided borrows are threaded through automatically so a method may
//! borrow across its await points), required and provided trait methods, and
//! impl methods. Non-`async fn` items are left exactly as written. Method-level
//! generics and `impl Trait` arguments aren't handled yet — no orca trait needs
//! them — and would be added here when one does.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{
    FnArg, GenericParam, Item, Lifetime, LifetimeParam, ReturnType, Signature, Type, WhereClause,
    parse_quote, parse2,
};

pub fn expand(item: TokenStream2) -> TokenStream2 {
    match parse2::<Item>(item) {
        Ok(Item::Trait(mut it)) => {
            for m in it.items.iter_mut() {
                if let syn::TraitItem::Fn(f) = m
                    && rewrite_async_sig(&mut f.sig)
                    && let Some(block) = f.default.take()
                {
                    // Provided trait method: wrap its body.
                    f.default = Some(parse_quote!({ Box::pin(async move #block) }));
                }
            }
            quote!(#it)
        }
        Ok(Item::Impl(mut it)) => {
            for m in it.items.iter_mut() {
                if let syn::ImplItem::Fn(f) = m
                    && rewrite_async_sig(&mut f.sig)
                {
                    let block = &f.block;
                    f.block = parse_quote!({ Box::pin(async move #block) });
                }
            }
            quote!(#it)
        }
        Ok(other) => syn::Error::new_spanned(
            other,
            "#[orca_async] expects a trait definition or an impl block",
        )
        .to_compile_error(),
        Err(e) => e.to_compile_error(),
    }
}

/// Rewrite one signature in place if it is `async fn`. Returns whether it was
/// async (i.e. whether a body needs wrapping). Non-async signatures are left
/// untouched and return `false`.
fn rewrite_async_sig(sig: &mut Signature) -> bool {
    if sig.asyncness.take().is_none() {
        return false;
    }
    let at = Lifetime::new("'async_trait", Span::call_site());
    let mut fresh: Vec<Lifetime> = Vec::new();
    let mut counter = 0usize;

    // Assign a fresh lifetime to the receiver and to every elided reference
    // parameter, tying each to `'async_trait` so the returned future may borrow.
    for arg in sig.inputs.iter_mut() {
        match arg {
            FnArg::Receiver(r) => {
                if let Some((_amp, lt)) = &mut r.reference {
                    match lt {
                        Some(existing) => fresh.push(existing.clone()),
                        None => {
                            let l = new_lt(counter);
                            *lt = Some(l.clone());
                            fresh.push(l);
                            counter += 1;
                        }
                    }
                }
            }
            FnArg::Typed(pt) => assign_ref_lifetimes(&mut pt.ty, &mut counter, &mut fresh),
        }
    }

    let output: Type = match &sig.output {
        ReturnType::Default => parse_quote!(()),
        ReturnType::Type(_, t) => (**t).clone(),
    };

    // Prepend the fresh param lifetimes and `'async_trait` to the generics.
    let mut params: Punctuated<GenericParam, Comma> = Punctuated::new();
    for l in &fresh {
        params.push(GenericParam::Lifetime(LifetimeParam::new(l.clone())));
    }
    params.push(GenericParam::Lifetime(LifetimeParam::new(at.clone())));
    params.extend(sig.generics.params.iter().cloned());
    sig.generics.params = params;

    let where_clause = sig
        .generics
        .where_clause
        .get_or_insert_with(|| WhereClause {
            where_token: Default::default(),
            predicates: Punctuated::new(),
        });
    for l in &fresh {
        where_clause.predicates.push(parse_quote!(#l: #at));
    }
    where_clause.predicates.push(parse_quote!(Self: #at));

    sig.output = parse_quote!(
        -> ::core::pin::Pin<Box<dyn ::core::future::Future<Output = #output> + ::core::marker::Send + #at>>
    );
    true
}

/// Give the outer reference of `ty` a fresh lifetime if it lacks one, recursing
/// through nested references (`&&T`). Non-reference types are left as-is; their
/// own lifetime args (e.g. `Cow<'a, str>`) are the caller's responsibility and
/// none of the orca traits use them.
fn assign_ref_lifetimes(ty: &mut Type, counter: &mut usize, out: &mut Vec<Lifetime>) {
    if let Type::Reference(r) = ty {
        match &r.lifetime {
            Some(existing) => out.push(existing.clone()),
            None => {
                let l = new_lt(*counter);
                r.lifetime = Some(l.clone());
                out.push(l);
                *counter += 1;
            }
        }
        assign_ref_lifetimes(&mut r.elem, counter, out);
    }
}

fn new_lt(n: usize) -> Lifetime {
    Lifetime::new(&format!("'life{n}"), Span::call_site())
}
