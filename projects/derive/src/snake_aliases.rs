//! `#[snake_aliases]` — accept each multi-word field's snake_case spelling as a
//! serde alias on a camelCase wire type.
//!
//! Peers on an older release, plugin binaries built against an older ABI, and
//! JSON persisted before the camelCase rename all still emit snake_case keys.
//! Without the alias those keys are silently ignored (every field here is
//! `default`-able or optional), so decode "succeeds" with the data dropped.
//! Remove once the fleet and every plugin have been rolled past the rename.
//!
//! `#[camel_aliases]` is the mirror for types whose wire form stays snake_case
//! for one more release (because an older peer or plugin would otherwise drop
//! the camelCase keys): it accepts each field's camelCase spelling.
//!
//! On an enum, the fields of every struct-like variant get the same treatment
//! (for `rename_all_fields = "camelCase"`).
//!
//! Left alone: `rename`d fields (their wire name never came from the ident),
//! `flatten`ed fields (serde rejects an alias there), skipped fields (never
//! decoded), and fields that already carry the identical alias.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Data, DeriveInput, Field, Fields, Ident};

/// Which spelling the added alias carries.
#[derive(Clone, Copy)]
pub(crate) enum Case {
    Snake,
    Camel,
}

pub(crate) fn expand(mut input: DeriveInput, case: Case) -> TokenStream2 {
    match &mut input.data {
        Data::Struct(data) => add_aliases(&mut data.fields, case),
        Data::Enum(data) => {
            for variant in data.variants.iter_mut() {
                add_aliases(&mut variant.fields, case);
            }
        }
        Data::Union(_) => {
            return syn::Error::new_spanned(
                &input.ident,
                "#[snake_aliases]/#[camel_aliases] apply to structs and enums only",
            )
            .to_compile_error();
        }
    }
    quote!(#input)
}

fn add_aliases(fields: &mut Fields, case: Case) {
    let Fields::Named(named) = fields else {
        return;
    };
    for field in named.named.iter_mut() {
        let Some(ident) = field.ident.clone() else {
            continue;
        };
        let snake = snake_name(&ident);
        if !snake.contains('_') {
            continue;
        }
        let alias = match case {
            Case::Snake => snake,
            Case::Camel => camel_name(&snake),
        };
        if leaves_alone(field, &alias) {
            continue;
        }
        field
            .attrs
            .push(syn::parse_quote!(#[serde(alias = #alias)]));
    }
}

/// serde's `rename_all = "camelCase"` spelling of a snake_case ident.
fn camel_name(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn snake_name(ident: &Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_string()
}

/// `#[serde(alias = "<ident>")]` for a multi-word field, or `None` when the
/// camelCase and snake_case spellings coincide.
pub(crate) fn alias_attr(ident: &Ident) -> Option<TokenStream2> {
    let name = snake_name(ident);
    name.contains('_').then(|| quote!(#[serde(alias = #name)]))
}

fn leaves_alone(field: &Field, alias: &str) -> bool {
    field
        .attrs
        .iter()
        .any(|attr| serde_attr_leaves_alone(attr, alias))
}

fn serde_attr_leaves_alone(attr: &Attribute, alias: &str) -> bool {
    if !attr.path().is_ident("serde") {
        return false;
    }
    let mut skip = false;
    let parsed = attr.parse_nested_meta(|meta| {
        let key = meta
            .path
            .get_ident()
            .map(|i| i.to_string())
            .unwrap_or_default();
        if meta.input.peek(syn::Token![=]) {
            let value: syn::Expr = meta.value()?.parse()?;
            let same_alias = matches!(
                &value,
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) if s.value() == alias
            );
            if key == "rename" || (key == "alias" && same_alias) {
                skip = true;
            }
        } else if meta.input.peek(syn::token::Paren) {
            let content;
            syn::parenthesized!(content in meta.input);
            let _: TokenStream2 = content.parse()?;
            if key == "rename" {
                skip = true;
            }
        } else if matches!(key.as_str(), "flatten" | "skip" | "skip_deserializing") {
            skip = true;
        }
        Ok(())
    });
    // An unparseable `#[serde]` is serde's error to report; adding an alias
    // next to it would only add a second, more confusing one.
    skip || parsed.is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: TokenStream2) -> String {
        expand(syn::parse2(src).unwrap(), Case::Snake).to_string()
    }

    #[test]
    fn multi_word_fields_get_their_snake_name_as_an_alias() {
        let out = run(quote! {
            #[serde(rename_all = "camelCase")]
            struct S { peer_id: String, ok: bool }
        });
        assert!(out.contains("alias = \"peer_id\""), "{out}");
        assert!(!out.contains("alias = \"ok\""), "{out}");
    }

    #[test]
    fn renamed_flattened_and_skipped_fields_are_left_alone() {
        let out = run(quote! {
            struct S {
                #[serde(rename = "x")] a_b: u8,
                #[serde(flatten)] e_f: Inner,
                #[serde(skip)] i_j: u8,
                #[serde(skip_deserializing)] k_l: u8,
                #[serde(default = "f", skip_serializing_if = "Option::is_none")] g_h: Option<u8>,
            }
        });
        for left_alone in ["a_b", "e_f", "i_j", "k_l"] {
            assert!(!out.contains(&format!("alias = \"{left_alone}\"")), "{out}");
        }
        assert!(out.contains("alias = \"g_h\""), "{out}");
    }

    #[test]
    fn an_existing_different_alias_still_gets_the_snake_one() {
        let out = run(quote! {
            struct S {
                #[serde(default, alias = "old")] c_d: u8,
                #[serde(alias = "m_n")] m_n: u8,
            }
        });
        assert!(out.contains("alias = \"c_d\""), "{out}");
        assert_eq!(out.matches("alias = \"m_n\"").count(), 1, "{out}");
    }

    #[test]
    fn struct_variant_fields_are_aliased() {
        let out = run(quote! {
            #[serde(tag = "kind", rename_all_fields = "camelCase")]
            enum E { A { window_start: u64, count: u32 }, B(u8), C }
        });
        assert!(out.contains("alias = \"window_start\""), "{out}");
        assert!(!out.contains("alias = \"count\""), "{out}");
    }

    #[test]
    fn camel_mode_adds_the_camel_spelling() {
        let out = expand(
            syn::parse2(quote! { struct S { lan_v4: u8, self_only: bool, ok: bool } }).unwrap(),
            Case::Camel,
        )
        .to_string();
        assert!(out.contains("alias = \"lanV4\""), "{out}");
        assert!(out.contains("alias = \"selfOnly\""), "{out}");
        assert!(!out.contains("alias = \"ok\""), "{out}");
    }

    #[test]
    fn alias_attr_is_none_for_single_word_idents() {
        let one: Ident = syn::parse_quote!(name);
        let two: Ident = syn::parse_quote!(base_url);
        assert!(alias_attr(&one).is_none());
        assert!(
            alias_attr(&two)
                .unwrap()
                .to_string()
                .contains("\"base_url\"")
        );
    }

    #[test]
    fn unions_are_rejected() {
        let out = run(quote! { union U { a: u8 } });
        assert!(out.contains("compile_error"), "{out}");
    }
}
