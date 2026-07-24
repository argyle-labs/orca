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
//! Parses the struct, extracts fields (respecting `#[secret]` and `Option<T>`),
//! then delegates to `endpoint_resource::expand()`.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{
    Expr, ExprLit, Fields, Ident, ItemStruct, Lit, LitStr, MetaNameValue, Token,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

use crate::endpoint_resource::{EndpointField, EndpointResource, unwrap_option};

pub(crate) struct EndpointResourceAttr {
    pub(crate) plugin: LitStr,
    pub(crate) table: Option<String>,
    /// Crate path the macro emits against. Defaults to `::plugin_toolkit`;
    /// domain crates pass `crate = ::macro_runtime` to anchor against the
    /// lower-level macro-runtime crate.
    pub(crate) crate_path: syn::Path,
    /// Opt-in mesh replication: the last-write-wins column name (`"updated_at"`).
    /// See [`crate::endpoint_resource::EndpointResource::lww`].
    pub(crate) lww: Option<String>,
}

impl Parse for EndpointResourceAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let items = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut plugin = None;
        let mut table = None;
        let mut crate_path: Option<syn::Path> = None;
        let mut lww = None;
        for nv in items {
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            if key == "crate" {
                match &nv.value {
                    Expr::Path(p) => crate_path = Some(p.path.clone()),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "expected a path (e.g. `::plugin_toolkit` or `::macro_runtime`)",
                        ));
                    }
                }
                continue;
            }
            let val = match &nv.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) => s.clone(),
                _ => {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "expected string literal",
                    ));
                }
            };
            match key.as_str() {
                "plugin" => plugin = Some(val),
                "table" => table = Some(val.value()),
                "lww" => lww = Some(val.value()),
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!("unknown key `{other}`; expected: plugin, table, lww, crate"),
                    ));
                }
            }
        }
        Ok(Self {
            plugin: plugin
                .ok_or_else(|| syn::Error::new(Span::call_site(), "missing `plugin = \"...\"`"))?,
            table,
            crate_path: crate_path.unwrap_or_else(|| syn::parse_quote!(::plugin_toolkit)),
            lww,
        })
    }
}

pub(crate) fn expand(attr: EndpointResourceAttr, item: ItemStruct) -> syn::Result<TokenStream2> {
    let named = match &item.fields {
        Fields::Named(n) => &n.named,
        _ => {
            return Err(syn::Error::new_spanned(
                &item.ident,
                "#[endpoint_resource] requires a struct with named fields",
            ));
        }
    };

    // `name`, `addresses`, and `enabled` are implicit fields managed by the
    // macro (PK / built-in connection-fallback list / toggle). The user may
    // include them for documentation clarity but they're filtered out.
    let data_fields = named.iter().filter(|f| {
        let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
        n != "name" && n != "addresses" && n != "enabled"
    });

    let mut endpoint_fields: Vec<EndpointField> = Vec::new();
    for f in data_fields {
        let name: Ident = f
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
                    "only `#[secret]` and `#[doc]` are recognised on endpoint_resource fields",
                ));
            }
        }
        let (optional, ty) = unwrap_option(f.ty.clone());
        endpoint_fields.push(EndpointField {
            secret,
            optional,
            name,
            ty,
        });
    }

    let plugin_str = attr.plugin.value();
    let table = attr
        .table
        .unwrap_or_else(|| format!("{}_endpoints", plugin_str.replace('-', "_")));

    let resource = EndpointResource {
        plugin: attr.plugin,
        table,
        fields: endpoint_fields,
        crate_path: attr.crate_path,
        lww: attr.lww,
    };

    // The struct definition is consumed — we emit nothing from it.
    // The macro generates EndpointRow, EndpointEntry, endpoint_db, and tools.
    // Emit a type alias so the struct's original name stays usable.
    let struct_ident = &item.ident;
    let alias = if struct_ident != "EndpointRow" {
        quote! { pub type #struct_ident = EndpointRow; }
    } else {
        quote! {}
    };

    let expanded = crate::endpoint_resource::expand(resource)?;
    Ok(quote! { #expanded #alias })
}
