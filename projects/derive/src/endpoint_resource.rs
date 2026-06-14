//! `endpoint_resource!` — function-like proc-macro that emits the full
//! 5-verb REST surface for an endpoint-registry resource.
//!
//! See [[feedback-plugin-toolkit-max-power-min-boilerplate]] for the
//! design principle: plugin code expresses maximum functionality with
//! minimum boilerplate; the macro generates the row struct, db helpers,
//! schema fragment, args/output types, and `#[orca_tool]` functions.
//!
//! ## Input
//!
//! ```rust,ignore
//! endpoint_resource! {
//!     plugin: "dockge",
//!     fields: {
//!         base_url: String,
//!         #[secret] token: String,
//!     }
//! }
//! ```
//!
//! ## Output
//!
//! For plugin name `"dockge"`:
//! - `pub struct EndpointRow { name, base_url, token, enabled }` in the
//!   calling crate.
//! - `pub mod endpoint_db { list, get, insert, update, upsert, remove }`
//!   with rusqlite-backed implementations.
//! - One `inventory::submit!` of `db::SchemaFragment` so
//!   `db::open_default()` creates the table.
//! - Five `#[orca_tool]`-annotated async fns under `dockge.{list, detail,
//!   create, update, delete}`, each emitting CLI + MCP + REST surfaces.
//! - Args/Output structs per verb with serde / clap / schemars derives.
//!
//! Fields marked `#[secret]` are stored in the row and accepted on create
//! / update but skipped from the public read-side `EndpointEntry` so they
//! never round-trip through the `.list` / `.detail` surfaces.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    Attribute, Ident, LitStr, Path, Token, Type,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

/// One field of the endpoint resource. Mirrors a struct field shape but
/// only `#[secret]` is recognised as an attribute.
pub(crate) struct EndpointField {
    pub(crate) secret: bool,
    /// True when the original type was `Option<T>`; `ty` is the unwrapped `T`.
    pub(crate) optional: bool,
    pub(crate) name: Ident,
    /// The inner type `T` (unwrapped from `Option<T>` when `optional` is true).
    pub(crate) ty: Type,
}

/// Unwrap `Option<T>` → `(true, T)`, anything else → `(false, ty)`.
pub(crate) fn unwrap_option(ty: &Type) -> (bool, Type) {
    if let Type::Path(tp) = ty
        && let Some(last) = tp.path.segments.last()
        && last.ident == "Option"
        && let syn::PathArguments::AngleBracketed(ref args) = last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (true, inner.clone());
    }
    (false, ty.clone())
}

impl Parse for EndpointField {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs: Vec<Attribute> = input.call(Attribute::parse_outer)?;
        let mut secret = false;
        for attr in &attrs {
            if attr.path().is_ident("secret") {
                secret = true;
            } else {
                return Err(syn::Error::new_spanned(
                    attr,
                    "endpoint_resource! field attributes: only `#[secret]` is recognised",
                ));
            }
        }
        let name: Ident = input.parse()?;
        let _: Token![:] = input.parse()?;
        let raw_ty: Type = input.parse()?;
        let (optional, ty) = unwrap_option(&raw_ty);
        Ok(Self {
            secret,
            optional,
            name,
            ty,
        })
    }
}

/// Top-level input to `endpoint_resource!`. Keys: `plugin`, `fields`,
/// optional `table` (defaults to `<plugin>_endpoints`).
pub(crate) struct EndpointResource {
    pub(crate) plugin: LitStr,
    pub(crate) table: String,
    pub(crate) fields: Vec<EndpointField>,
}

impl Parse for EndpointResource {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut plugin: Option<LitStr> = None;
        let mut table: Option<String> = None;
        let mut fields: Option<Vec<EndpointField>> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            let _: Token![:] = input.parse()?;
            match key.to_string().as_str() {
                "plugin" => {
                    plugin = Some(input.parse()?);
                }
                "table" => {
                    let s: LitStr = input.parse()?;
                    table = Some(s.value());
                }
                "fields" => {
                    let content;
                    syn::braced!(content in input);
                    let parsed: Punctuated<EndpointField, Token![,]> =
                        Punctuated::parse_terminated(&content)?;
                    fields = Some(parsed.into_iter().collect());
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown key `{other}`; expected one of: plugin, table, fields"),
                    ));
                }
            }
            if input.is_empty() {
                break;
            }
            let _: Token![,] = input.parse()?;
        }

        let plugin = plugin
            .ok_or_else(|| syn::Error::new(Span::call_site(), "missing `plugin: \"...\"`"))?;
        let fields = fields
            .ok_or_else(|| syn::Error::new(Span::call_site(), "missing `fields: { ... }`"))?;
        let table = table.unwrap_or_else(|| format!("{}_endpoints", plugin.value()));

        Ok(Self {
            plugin,
            table,
            fields,
        })
    }
}

/// Convert "dockge" → "Dockge", "home_assistant" → "HomeAssistant".
fn pascal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
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

/// Map a Rust type ident → SQLite column type. MVP supports the types
/// `EndpointRow` actually carries today (String + bool); anything else
/// falls back to `TEXT NOT NULL` with a compile-error hint.
fn sql_type_for(ty: &Type) -> syn::Result<&'static str> {
    let path = match ty {
        Type::Path(tp) => &tp.path,
        _ => {
            return Err(syn::Error::new_spanned(
                ty,
                "endpoint_resource!: field type must be a path (e.g. `String`)",
            ));
        }
    };
    let last = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, "endpoint_resource!: empty type path"))?;
    Ok(match last.ident.to_string().as_str() {
        "String" => "TEXT NOT NULL",
        "bool" => "INTEGER NOT NULL DEFAULT 0",
        "i64" | "u64" | "u32" | "i32" => "INTEGER NOT NULL DEFAULT 0",
        other => {
            return Err(syn::Error::new_spanned(
                ty,
                format!(
                    "endpoint_resource!: unsupported field type `{other}`; supported: String, bool, i32/i64/u32/u64"
                ),
            ));
        }
    })
}

pub(crate) fn expand(input: EndpointResource) -> syn::Result<TokenStream2> {
    let plugin_str = input.plugin.value();
    let plugin_pascal = pascal(&plugin_str);
    let table = &input.table;

    let entry_ident = format_ident!("EndpointEntry");
    let row_ident = format_ident!("EndpointRow");

    // Per-verb names: `DockgeListArgs`, `DockgeListOutput`, etc., plus
    // snake_case function idents the underlying #[orca_tool] derives from.
    let list_args = format_ident!("{plugin_pascal}ListArgs");
    let list_output = format_ident!("{plugin_pascal}ListOutput");
    let detail_args = format_ident!("{plugin_pascal}DetailArgs");
    let detail_output = format_ident!("{plugin_pascal}DetailOutput");
    let create_args = format_ident!("{plugin_pascal}CreateArgs");
    let create_output = format_ident!("{plugin_pascal}CreateOutput");
    let update_args = format_ident!("{plugin_pascal}UpdateArgs");
    let update_output = format_ident!("{plugin_pascal}UpdateOutput");
    let delete_args = format_ident!("{plugin_pascal}DeleteArgs");
    let delete_output = format_ident!("{plugin_pascal}DeleteOutput");

    let list_fn = format_ident!("{}_list", plugin_str);
    let detail_fn = format_ident!("{}_detail", plugin_str);
    let create_fn = format_ident!("{}_create", plugin_str);
    let update_fn = format_ident!("{}_update", plugin_str);
    let delete_fn = format_ident!("{}_delete", plugin_str);

    let field_idents: Vec<&Ident> = input.fields.iter().map(|f| &f.name).collect();

    // Storage types: optional fields wrap the inner T in Option<>.
    let row_field_ty_tokens: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let ty = &f.ty;
            if f.optional {
                quote! { Option<#ty> }
            } else {
                quote! { #ty }
            }
        })
        .collect();

    // Entry struct fields (public-side, no secrets).
    // secret + non-optional → excluded.
    // secret + optional     → has_<name>: bool.
    // non-secret + optional → <name>: Option<T>.
    // non-secret            → <name>: T.
    let mut entry_field_decls: Vec<TokenStream2> = Vec::new();
    let mut entry_from_row: Vec<TokenStream2> = Vec::new();
    for f in &input.fields {
        let fname = &f.name;
        let ty = &f.ty;
        if f.secret && !f.optional {
            // excluded from public entry
        } else if f.secret && f.optional {
            let has_name = format_ident!("has_{}", fname);
            entry_field_decls.push(quote! { pub #has_name: bool, });
            entry_from_row.push(quote! { #has_name: row.#fname.is_some(), });
        } else if f.optional {
            entry_field_decls.push(quote! { pub #fname: Option<#ty>, });
            entry_from_row.push(quote! { #fname: row.#fname.clone(), });
        } else {
            entry_field_decls.push(quote! { pub #fname: #ty, });
            entry_from_row.push(quote! { #fname: row.#fname.clone(), });
        }
    }

    // CreateArgs fields: optional storage fields stay Option<T> in the arg.
    let create_field_decls: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let fname = &f.name;
            let ty = &f.ty;
            if f.optional {
                quote! { #[arg(long)] pub #fname: Option<#ty>, }
            } else {
                quote! { #[arg(long)] pub #fname: #ty, }
            }
        })
        .collect();

    // UpdateArgs fields: all optional (PATCH). Inner type T regardless of storage optionality.
    let update_field_decls: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let fname = &f.name;
            let ty = &f.ty;
            quote! { #[arg(long)] pub #fname: Option<#ty>, }
        })
        .collect();

    // Update patch stanzas: optional storage fields wrap the value in Some().
    let update_patch_stanzas: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let fname = &f.name;
            let fname_str = fname.to_string();
            if f.optional {
                quote! {
                    if let ::std::option::Option::Some(v) = args.#fname {
                        row.#fname = ::std::option::Option::Some(v);
                        applied.push(#fname_str.to_string());
                    }
                }
            } else {
                quote! {
                    if let ::std::option::Option::Some(v) = args.#fname {
                        row.#fname = v;
                        applied.push(#fname_str.to_string());
                    }
                }
            }
        })
        .collect();

    // ── SQL ───────────────────────────────────────────────────────────────
    // CREATE TABLE statement registered into the db crate's schema fragment
    // inventory. Columns are ordered: name (PK), <fields>, enabled, created_at.
    let mut create_columns = String::from("name TEXT PRIMARY KEY,\n");
    for f in &input.fields {
        let base_sql_type = sql_type_for(&f.ty)?;
        // Optional fields drop NOT NULL (and DEFAULT 0 for bools/ints).
        let sql_type = if f.optional {
            base_sql_type
                .trim_end_matches(" NOT NULL DEFAULT 0")
                .trim_end_matches(" NOT NULL")
        } else {
            base_sql_type
        };
        create_columns.push_str(&format!("    {} {},\n", f.name, sql_type));
    }
    create_columns.push_str("    enabled INTEGER NOT NULL DEFAULT 1,\n");
    create_columns
        .push_str("    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))\n");
    let create_table_sql = format!("CREATE TABLE IF NOT EXISTS {table} (\n    {create_columns});");

    let select_cols = std::iter::once("name".to_string())
        .chain(input.fields.iter().map(|f| f.name.to_string()))
        .chain(std::iter::once("enabled".to_string()))
        .collect::<Vec<_>>()
        .join(", ");

    let list_sql = format!("SELECT {select_cols} FROM {table} ORDER BY name");
    let get_sql = format!("SELECT {select_cols} FROM {table} WHERE name = ?1");
    let insert_placeholders = (1..=(input.fields.len() + 2))
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!("INSERT INTO {table} ({select_cols}) VALUES ({insert_placeholders})");
    let update_assignments = input
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{} = ?{}", f.name, i + 2))
        .chain(std::iter::once(format!(
            "enabled = ?{}",
            input.fields.len() + 2
        )))
        .collect::<Vec<_>>()
        .join(", ");
    let update_sql = format!("UPDATE {table} SET {update_assignments} WHERE name = ?1");
    let upsert_set = input
        .fields
        .iter()
        .map(|f| format!("{} = excluded.{}", f.name, f.name))
        .chain(std::iter::once("enabled = excluded.enabled".to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let upsert_sql = format!(
        "INSERT INTO {table} ({select_cols}) VALUES ({insert_placeholders}) \
         ON CONFLICT(name) DO UPDATE SET {upsert_set}"
    );
    let delete_sql = format!("DELETE FROM {table} WHERE name = ?1");

    // Row-construction tuple indices: `(0, 1, 2, ..., n+1)` where n = fields.len()
    let row_indices = (0..(input.fields.len() + 2))
        .map(syn::Index::from)
        .collect::<Vec<_>>();
    let row_name_idx = &row_indices[0];
    let row_field_indices = &row_indices[1..=input.fields.len()];
    let row_enabled_idx = &row_indices[input.fields.len() + 1];

    // ── Doc strings ───────────────────────────────────────────────────────
    let plugin_str_lit = LitStr::new(&plugin_str, Span::call_site());
    let list_doc = LitStr::new(
        &format!("List registered {plugin_str} endpoints."),
        Span::call_site(),
    );
    let detail_doc = LitStr::new(
        &format!("Detail for a single {plugin_str} endpoint."),
        Span::call_site(),
    );
    let create_doc = LitStr::new(
        &format!(
            "[MUTATES STATE] Register a new {plugin_str} endpoint. Errors if `name` is already taken — use {plugin_str}.update to modify an existing endpoint."
        ),
        Span::call_site(),
    );
    let update_doc = LitStr::new(
        &format!(
            "[MUTATES STATE] Modify an existing {plugin_str} endpoint. PATCH semantics — endpoint must already exist."
        ),
        Span::call_site(),
    );
    let delete_doc = LitStr::new(
        &format!(
            "[MUTATES STATE] Remove a registered {plugin_str} endpoint. Idempotent — returns `changed: false` if no row matched."
        ),
        Span::call_site(),
    );

    let toolkit: Path = syn::parse_quote!(::orca_plugin_toolkit);

    let expanded = quote! {
        // ── Row struct ───────────────────────────────────────────────────
        /// Endpoint row — the storage shape for this resource's table.
        /// `name` is the operator-chosen primary key; `enabled` defaults
        /// true on insert.
        #[derive(Debug, Clone)]
        pub struct #row_ident {
            pub name: ::std::string::String,
            #( pub #field_idents: #row_field_ty_tokens, )*
            pub enabled: bool,
        }

        // ── Schema fragment registration ─────────────────────────────────
        ::inventory::submit! {
            ::db::SchemaFragment {
                name: #table,
                sql: #create_table_sql,
            }
        }

        // ── DB CRUD module ───────────────────────────────────────────────
        pub mod endpoint_db {
            use super::#row_ident;
            use ::anyhow::Result;
            use ::rusqlite::{Connection, OptionalExtension};

            pub fn list(conn: &Connection) -> Result<::std::vec::Vec<#row_ident>> {
                let mut stmt = conn.prepare(#list_sql)?;
                let rows = stmt.query_map([], |row| {
                    Ok(#row_ident {
                        name: row.get(#row_name_idx)?,
                        #( #field_idents: row.get(#row_field_indices)?, )*
                        enabled: row.get::<_, i32>(#row_enabled_idx)? != 0,
                    })
                })?;
                rows.collect::<::rusqlite::Result<::std::vec::Vec<_>>>()
                    .map_err(Into::into)
            }

            pub fn get(conn: &Connection, name: &str) -> Result<::std::option::Option<#row_ident>> {
                conn.query_row(
                    #get_sql,
                    ::rusqlite::params![name],
                    |row| {
                        Ok(#row_ident {
                            name: row.get(#row_name_idx)?,
                            #( #field_idents: row.get(#row_field_indices)?, )*
                            enabled: row.get::<_, i32>(#row_enabled_idx)? != 0,
                        })
                    },
                )
                .optional()
                .map_err(Into::into)
            }

            pub fn insert(conn: &Connection, ep: &#row_ident) -> Result<()> {
                conn.execute(
                    #insert_sql,
                    ::rusqlite::params![ep.name, #( ep.#field_idents, )* ep.enabled],
                )?;
                Ok(())
            }

            pub fn update(conn: &Connection, ep: &#row_ident) -> Result<bool> {
                let n = conn.execute(
                    #update_sql,
                    ::rusqlite::params![ep.name, #( ep.#field_idents, )* ep.enabled],
                )?;
                Ok(n > 0)
            }

            pub fn upsert(conn: &Connection, ep: &#row_ident) -> Result<()> {
                conn.execute(
                    #upsert_sql,
                    ::rusqlite::params![ep.name, #( ep.#field_idents, )* ep.enabled],
                )?;
                Ok(())
            }

            pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
                let n = conn.execute(
                    #delete_sql,
                    ::rusqlite::params![name],
                )?;
                Ok(n > 0)
            }
        }

        // ── Public-side endpoint entry (no secrets) ──────────────────────
        #[derive(::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema, Debug, Clone)]
        #[serde(rename_all = "camelCase")]
        pub struct #entry_ident {
            pub name: ::std::string::String,
            #( #entry_field_decls )*
            pub enabled: bool,
        }

        // ── list ─────────────────────────────────────────────────────────
        #[derive(::clap::Args, ::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema, Default)]
        #[serde(default)]
        pub struct #list_args {}

        #[derive(::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema, Default)]
        #[serde(default)]
        pub struct #list_output {
            pub endpoints: ::std::vec::Vec<#entry_ident>,
        }

        #[doc = #list_doc]
        #[::derive::orca_tool(domain = #plugin_str_lit, verb = "list")]
        async fn #list_fn(
            _args: #list_args,
            _ctx: &::contract::ToolCtx,
        ) -> ::anyhow::Result<#list_output> {
            let conn = #toolkit::runtime::open_db()?;
            let endpoints = endpoint_db::list(&conn)?
                .into_iter()
                .map(|row| #entry_ident {
                    name: row.name.clone(),
                    #( #entry_from_row )*
                    enabled: row.enabled,
                })
                .collect();
            Ok(#list_output { endpoints })
        }

        // ── detail ───────────────────────────────────────────────────────
        #[derive(::clap::Args, ::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #detail_args {
            /// Endpoint name to look up.
            #[arg(long)]
            pub name: ::std::string::String,
        }

        #[derive(::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #detail_output {
            pub endpoint: #entry_ident,
        }

        #[doc = #detail_doc]
        #[::derive::orca_tool(domain = #plugin_str_lit, verb = "detail")]
        async fn #detail_fn(
            args: #detail_args,
            _ctx: &::contract::ToolCtx,
        ) -> ::anyhow::Result<#detail_output> {
            let conn = #toolkit::runtime::open_db()?;
            let row = endpoint_db::get(&conn, &args.name)?
                .ok_or_else(|| #toolkit::runtime::missing_row_error(#plugin_str_lit, &args.name))?;
            Ok(#detail_output {
                endpoint: #entry_ident {
                    name: row.name.clone(),
                    #( #entry_from_row )*
                    enabled: row.enabled,
                },
            })
        }

        // ── create ───────────────────────────────────────────────────────
        #[derive(::clap::Args, ::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #create_args {
            /// Unique endpoint name (operator-chosen identifier).
            #[arg(long)]
            pub name: ::std::string::String,
            #( #create_field_decls )*
        }

        #[derive(::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #create_output {
            pub endpoint: #entry_ident,
        }

        #[doc = #create_doc]
        #[::derive::orca_tool(domain = #plugin_str_lit, verb = "create")]
        async fn #create_fn(
            args: #create_args,
            _ctx: &::contract::ToolCtx,
        ) -> ::anyhow::Result<#create_output> {
            let row = #row_ident {
                name: args.name.clone(),
                #( #field_idents: args.#field_idents, )*
                enabled: true,
            };
            let conn = #toolkit::runtime::open_db()?;
            endpoint_db::insert(&conn, &row)
                .map_err(|e| #toolkit::runtime::map_insert_conflict(e, #plugin_str_lit, &row.name))?;
            Ok(#create_output {
                endpoint: #entry_ident {
                    name: row.name.clone(),
                    #( #entry_from_row )*
                    enabled: row.enabled,
                },
            })
        }

        // ── update ───────────────────────────────────────────────────────
        // clap's `value_parser!` is a declarative macro that arm-matches on
        // the TOKEN TREE of the field type. Fully-qualified `::std::option::
        // Option<T>` doesn't match its `Option<$ty>` arm, so we emit bare
        // `Option<T>` here — the std prelude resolves it correctly at any
        // call site that imports the toolkit prelude (or no prelude at all).
        #[derive(::clap::Args, ::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema, Default)]
        #[serde(default)]
        pub struct #update_args {
            /// Endpoint name (required — PATCH targets by name).
            #[arg(long)]
            pub name: ::std::string::String,
            #( #update_field_decls )*
            #[arg(long)]
            pub enabled: Option<bool>,
        }

        #[derive(::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #update_output {
            pub endpoint: #entry_ident,
            pub applied: ::std::vec::Vec<::std::string::String>,
        }

        #[doc = #update_doc]
        #[::derive::orca_tool(domain = #plugin_str_lit, verb = "update")]
        async fn #update_fn(
            args: #update_args,
            _ctx: &::contract::ToolCtx,
        ) -> ::anyhow::Result<#update_output> {
            let conn = #toolkit::runtime::open_db()?;
            let mut row = endpoint_db::get(&conn, &args.name)?
                .ok_or_else(|| #toolkit::runtime::missing_row_error(#plugin_str_lit, &args.name))?;
            let mut applied: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
            #( #update_patch_stanzas )*
            if let ::std::option::Option::Some(v) = args.enabled {
                row.enabled = v;
                applied.push("enabled".to_string());
            }
            if applied.is_empty() {
                ::anyhow::bail!("no fields to update; pass at least one of the optional flags");
            }
            let changed = endpoint_db::update(&conn, &row)?;
            if !changed {
                ::anyhow::bail!("update reported no row change for `{}`", row.name);
            }
            Ok(#update_output {
                endpoint: #entry_ident {
                    name: row.name.clone(),
                    #( #entry_from_row )*
                    enabled: row.enabled,
                },
                applied,
            })
        }

        // ── delete ───────────────────────────────────────────────────────
        #[derive(::clap::Args, ::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #delete_args {
            /// Endpoint name to remove.
            #[arg(long)]
            pub name: ::std::string::String,
        }

        #[derive(::serde::Serialize, ::serde::Deserialize, ::schemars::JsonSchema)]
        pub struct #delete_output {
            pub name: ::std::string::String,
            pub changed: bool,
        }

        #[doc = #delete_doc]
        #[::derive::orca_tool(domain = #plugin_str_lit, verb = "delete")]
        async fn #delete_fn(
            args: #delete_args,
            _ctx: &::contract::ToolCtx,
        ) -> ::anyhow::Result<#delete_output> {
            let conn = #toolkit::runtime::open_db()?;
            let changed = endpoint_db::remove(&conn, &args.name)?;
            Ok(#delete_output {
                name: args.name,
                changed,
            })
        }
    };

    Ok(expanded)
}
