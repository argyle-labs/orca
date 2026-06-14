//! Attribute form of `endpoint_resource`:
//!
//! ```rust,ignore
//! #[endpoint_resource(plugin = "ntfy")]
//! pub struct NtfyEndpoint {
//!     pub name: String,
//!     pub base_url: String,
//!     pub topic: String,
//!     #[secret]
//!     pub token: Option<String>,
//!     pub enabled: bool,
//! }
//! ```
//!
//! Parses the struct, extracts fields (respecting `#[secret]`), then delegates
//! to the shared `endpoint_resource::expand()` so both macro forms stay in
//! sync.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{
    Attribute, Field, Fields, Ident, ItemStruct, LitStr, Token, Type,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

use crate::endpoint_resource::{EndpointField, EndpointResource};

/// Parsed `#[endpoint_resource(plugin = "ntfy")]` attribute.
pub(crate) struct EndpointResourceAttr {
    pub(crate) plugin: LitStr,
    pub(crate) table: Option<String>,
}

impl Parse for EndpointResourceAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let items = Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut plugin = None;
        let mut table = None;
        for nv in items {
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            match key.as_str() {
                "plugin" => {
                    plugin = Some(match &nv.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => s.clone(),
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "expected string literal for `plugin`",
                            ));
                        }
                    });
                }
                "table" => {
                    table = Some(match &nv.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => s.value(),
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "expected string literal for `table`",
                            ));
                        }
                    });
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!(
                            "unknown key `{other}`; expected one of: plugin, table"
                        ),
                    ));
                }
            }
        }
        Ok(Self {
            plugin: plugin.ok_or_else(|| {
                syn::Error::new(Span::call_site(), "missing `plugin = \"...\"`")
            })?,
            table,
        })
    }
}

/// Returns true if the type is `Option<T>` (bare ident path form).
fn is_option(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        if let Some(last) = tp.path.segments.last() {
            return last.ident == "Option";
        }
    }
    false
}

/// Strip `#[secret]` (and any other orca-internal attrs) from a field's attrs,
/// returning the cleaned list.
fn strip_secret(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|a| !a.path().is_ident("secret"))
        .cloned()
        .collect()
}

/// Expand `#[endpoint_resource(plugin = "...")]` applied to a struct.
pub(crate) fn expand(
    attr: EndpointResourceAttr,
    item: ItemStruct,
) -> syn::Result<TokenStream2> {
    let named = match &item.fields {
        Fields::Named(n) => &n.named,
        _ => {
            return Err(syn::Error::new_spanned(
                &item.ident,
                "#[endpoint_resource] requires a struct with named fields",
            ));
        }
    };

    // Separate `name` and `enabled` (implicit PKs/toggles) from endpoint data fields.
    // We still allow them in the struct for documentation clarity, but they're
    // managed by the macro's generated schema/row structs.
    let data_fields: Vec<&Field> = named
        .iter()
        .filter(|f| {
            let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
            n != "name" && n != "enabled"
        })
        .collect();

    let mut endpoint_fields: Vec<EndpointField> = Vec::new();
    for f in data_fields {
        let name = f
            .ident
            .clone()
            .ok_or_else(|| syn::Error::new_spanned(f, "expected named field"))?;
        let mut secret = false;
        for a in &f.attrs {
            if a.path().is_ident("secret") {
                secret = true;
            } else if !a.path().is_ident("doc") && !a.path().is_ident("allow") {
                return Err(syn::Error::new_spanned(
                    a,
                    "only `#[secret]` and `#[doc]` are allowed on endpoint_resource fields",
                ));
            }
        }
        endpoint_fields.push(EndpointField {
            secret,
            name,
            ty: f.ty.clone(),
            optional: is_option(&f.ty),
        });
    }

    // Build the EndpointResource input and delegate to the shared expand().
    let plugin_str = attr.plugin.value();
    let table = attr
        .table
        .unwrap_or_else(|| format!("{}_endpoints", plugin_str.replace('-', "_")));

    let resource = EndpointResource {
        plugin: attr.plugin,
        table,
        fields: endpoint_fields,
    };

    // The struct itself is consumed by the macro — we emit nothing from it
    // (the row struct is regenerated by expand() as `EndpointRow`). To keep
    // the plugin author's struct name available as a type alias:
    let entry_alias = Ident::new("EndpointRow", Span::call_site());
    let struct_ident = &item.ident;
    // Strip orca-internal attrs from the original struct's fields for the alias.
    let cleaned_fields: Vec<TokenStream2> = named
        .iter()
        .map(|f| {
            let cleaned_attrs = strip_secret(&f.attrs);
            let vis = &f.vis;
            let name = &f.ident;
            let ty = &f.ty;
            quote! { #( #cleaned_attrs )* #vis #name: #ty, }
        })
        .collect();

    let expanded_resource = crate::endpoint_resource::expand(resource)?;

    // Re-export the generated EndpointRow under the struct's original name as
    // a type alias so existing code that imports the struct name keeps working.
    let alias = if struct_ident != "EndpointRow" {
        quote! { pub type #struct_ident = #entry_alias; }
    } else {
        quote! {}
    };

    Ok(quote! {
        #expanded_resource
        #alias
    })
}
