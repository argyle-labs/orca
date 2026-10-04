//! `#[snake_aliases]` — accept each multi-word field's snake_case spelling as a
//! serde alias on a camelCase wire type.
//!
//! Peers on an older release, plugin binaries built against an older ABI, and
//! JSON persisted before the camelCase rename all still emit snake_case keys.
//! Without the alias those keys are silently ignored (every field here is
//! `default`-able or optional), so decode "succeeds" with the data dropped.
//! Remove once the fleet and every plugin have been rolled past the rename.
//!
//! Fields that already carry `rename`/`alias`, and `flatten`ed fields (serde
//! rejects an alias there), are left alone.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Data, DeriveInput, Fields, Ident};

pub(crate) fn expand(mut input: DeriveInput) -> TokenStream2 {
    let Data::Struct(data) = &mut input.data else {
        return syn::Error::new_spanned(&input.ident, "#[snake_aliases] applies to structs only")
            .to_compile_error();
    };
    if let Fields::Named(named) = &mut data.fields {
        for field in named.named.iter_mut() {
            if field.attrs.iter().any(opts_out) {
                continue;
            }
            if let Some(attr) = field.ident.as_ref().and_then(alias_attr) {
                field.attrs.push(syn::parse_quote!(#attr));
            }
        }
    }
    quote!(#input)
}

/// `#[serde(alias = "<ident>")]` for a multi-word field, or `None` when the
/// camelCase and snake_case spellings coincide.
pub(crate) fn alias_attr(ident: &Ident) -> Option<TokenStream2> {
    let name = ident.to_string();
    let name = name.strip_prefix("r#").unwrap_or(&name);
    name.contains('_').then(|| quote!(#[serde(alias = #name)]))
}

fn opts_out(attr: &Attribute) -> bool {
    if !attr.path().is_ident("serde") {
        return false;
    }
    let mut skip = false;
    let parsed = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("rename")
            || meta.path.is_ident("alias")
            || meta.path.is_ident("flatten")
        {
            skip = true;
        }
        if meta.input.peek(syn::Token![=]) {
            let _: syn::Expr = meta.value()?.parse()?;
        } else if meta.input.peek(syn::token::Paren) {
            let content;
            syn::parenthesized!(content in meta.input);
            let _: TokenStream2 = content.parse()?;
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
        expand(syn::parse2(src).unwrap()).to_string()
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
    fn renamed_aliased_and_flattened_fields_are_left_alone() {
        let out = run(quote! {
            struct S {
                #[serde(rename = "x")] a_b: u8,
                #[serde(default, alias = "old")] c_d: u8,
                #[serde(flatten)] e_f: Inner,
                #[serde(default = "f", skip_serializing_if = "Option::is_none")] g_h: Option<u8>,
            }
        });
        assert!(!out.contains("alias = \"a_b\""), "{out}");
        assert!(!out.contains("alias = \"c_d\""), "{out}");
        assert!(!out.contains("alias = \"e_f\""), "{out}");
        assert!(out.contains("alias = \"g_h\""), "{out}");
    }

    #[test]
    fn enums_are_rejected() {
        let out = run(quote! { enum E { A } });
        assert!(out.contains("compile_error"), "{out}");
    }
}
