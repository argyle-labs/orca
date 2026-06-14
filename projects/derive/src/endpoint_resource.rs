//! `endpoint_resource!` and `#[endpoint_resource]` — generates the full
//! 5-verb REST surface for an endpoint-registry resource.
//!
//! See [[feedback-plugin-toolkit-max-power-min-boilerplate]].
//!
//! Both forms generate identical output:
//! - `pub struct EndpointRow { name, <fields>, enabled }`
//! - `pub mod endpoint_db { list, get, insert, update, upsert, remove }`
//! - `inventory::submit!` of `db_types::SchemaFragment`
//! - Five `#[orca_tool]` async fns: `<plugin>.{list, detail, create, update, delete}`
//!
//! `#[secret]` fields: excluded from `EndpointEntry` (stored only).
//! `Option<T>` + `#[secret]` fields: appear as `has_<name>: bool` in entry.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    Attribute, Ident, LitStr, Path, Token, Type,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

pub(crate) struct EndpointField {
    pub(crate) secret: bool,
    /// True when storage type is `Option<T>`. `ty` holds the inner `T`.
    pub(crate) optional: bool,
    pub(crate) name: Ident,
    /// Inner type `T` (unwrapped from `Option<T>` when optional).
    pub(crate) ty: Type,
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
        let ty: Type = input.parse()?;
        let (optional, ty) = unwrap_option(ty);
        Ok(Self {
            secret,
            optional,
            name,
            ty,
        })
    }
}

/// Unwrap `Option<T>` → `(true, T)`, anything else → `(false, ty)`.
pub(crate) fn unwrap_option(ty: Type) -> (bool, Type) {
    if let Type::Path(ref tp) = ty
        && let Some(last) = tp.path.segments.last()
        && last.ident == "Option"
        && let syn::PathArguments::AngleBracketed(ref args) = last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (true, inner.clone());
    }
    (false, ty)
}

pub(crate) struct EndpointResource {
    pub(crate) plugin: LitStr,
    pub(crate) table: String,
    pub(crate) fields: Vec<EndpointField>,
    /// Crate path the macro emits against. Defaults to `::plugin_toolkit`;
    /// domain crates pass `crate = ::macro_runtime`.
    pub(crate) crate_path: syn::Path,
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
                "plugin" => plugin = Some(input.parse()?),
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
        let table =
            table.unwrap_or_else(|| format!("{}_endpoints", plugin.value().replace('-', "_")));
        Ok(Self {
            plugin,
            table,
            fields,
            // Function-macro form (`endpoint_resource! { … }`) doesn't currently
            // accept a `crate = ::path` key — callers are plugin-side and
            // anchor to `::plugin_toolkit` unconditionally. Add a key here when
            // a domain-crate use of the function-form arises.
            crate_path: syn::parse_quote!(::plugin_toolkit),
        })
    }
}

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
                    "endpoint_resource!: unsupported type `{other}`; supported: String, bool, i32/i64/u32/u64"
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

    let plugin_ident_str = plugin_str.replace('-', "_");
    let list_fn = format_ident!("{}_list", plugin_ident_str);
    let detail_fn = format_ident!("{}_detail", plugin_ident_str);
    let create_fn = format_ident!("{}_create", plugin_ident_str);
    let update_fn = format_ident!("{}_update", plugin_ident_str);
    let delete_fn = format_ident!("{}_delete", plugin_ident_str);

    let field_idents: Vec<&Ident> = input.fields.iter().map(|f| &f.name).collect();

    // ── Row struct field declarations ────────────────────────────────────
    let row_field_decls: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let n = &f.name;
            let ty = &f.ty;
            if f.optional {
                quote! { pub #n: Option<#ty>, }
            } else {
                quote! { pub #n: #ty, }
            }
        })
        .collect();

    // ── Entry struct (public read side) ─────────────────────────────────
    // secret+non-optional → excluded
    // secret+optional     → has_<name>: bool
    // non-secret+optional → Option<T>
    // non-secret          → T
    let entry_field_decls: Vec<TokenStream2> = input
        .fields
        .iter()
        .filter_map(|f| {
            let n = &f.name;
            let ty = &f.ty;
            if f.secret && !f.optional {
                None
            } else if f.secret && f.optional {
                let has = format_ident!("has_{}", n);
                Some(quote! { pub #has: bool, })
            } else if f.optional {
                Some(quote! { pub #n: Option<#ty>, })
            } else {
                Some(quote! { pub #n: #ty, })
            }
        })
        .collect();

    // Entry construction from a `row` binding.
    let entry_from_row: Vec<TokenStream2> = input
        .fields
        .iter()
        .filter_map(|f| {
            let n = &f.name;
            if f.secret && !f.optional {
                None
            } else if f.secret && f.optional {
                let has = format_ident!("has_{}", n);
                Some(quote! { #has: row.#n.is_some(), })
            } else {
                Some(quote! { #n: row.#n.clone(), })
            }
        })
        .collect();

    // ── CreateArgs fields ────────────────────────────────────────────────
    let create_field_decls: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let n = &f.name;
            let ty = &f.ty;
            if f.optional {
                quote! { #[arg(long)] pub #n: Option<#ty>, }
            } else {
                quote! { #[arg(long)] pub #n: #ty, }
            }
        })
        .collect();

    // Row construction from create args (field types match directly).
    let create_row_fields: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let n = &f.name;
            quote! { #n: args.#n, }
        })
        .collect();

    // ── UpdateArgs fields (all optional for PATCH) ───────────────────────
    let update_field_decls: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let n = &f.name;
            let ty = &f.ty;
            quote! { #[arg(long)] pub #n: Option<#ty>, }
        })
        .collect();

    // Patch stanzas: optional storage fields wrap value in Some().
    let update_patch_stanzas: Vec<TokenStream2> = input
        .fields
        .iter()
        .map(|f| {
            let n = &f.name;
            let ns = n.to_string();
            if f.optional {
                quote! {
                    if let ::std::option::Option::Some(v) = args.#n {
                        row.#n = ::std::option::Option::Some(v);
                        applied.push(#ns.to_string());
                    }
                }
            } else {
                quote! {
                    if let ::std::option::Option::Some(v) = args.#n {
                        row.#n = v;
                        applied.push(#ns.to_string());
                    }
                }
            }
        })
        .collect();

    // ── SQL ───────────────────────────────────────────────────────────────
    let mut create_columns = String::from("name TEXT PRIMARY KEY,\n");
    for f in &input.fields {
        let base = sql_type_for(&f.ty)?;
        // Optional fields: drop NOT NULL / DEFAULT suffix → just TEXT or INTEGER
        let col_type = if f.optional {
            base.split(' ').next().unwrap_or(base)
        } else {
            base
        };
        create_columns.push_str(&format!("    {} {},\n", f.name, col_type));
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

    let n_fields = input.fields.len();
    let insert_placeholders = (1..=(n_fields + 2))
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!("INSERT INTO {table} ({select_cols}) VALUES ({insert_placeholders})");

    let update_assignments = input
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{} = ?{}", f.name, i + 2))
        .chain(std::iter::once(format!("enabled = ?{}", n_fields + 2)))
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
    let list_sql = format!("SELECT {select_cols} FROM {table} ORDER BY name");
    let get_sql = format!("SELECT {select_cols} FROM {table} WHERE name = ?1");
    let delete_sql = format!("DELETE FROM {table} WHERE name = ?1");

    let row_indices = (0..(n_fields + 2))
        .map(syn::Index::from)
        .collect::<Vec<_>>();
    let row_name_idx = &row_indices[0];
    let row_field_indices = &row_indices[1..=n_fields];
    let row_enabled_idx = &row_indices[n_fields + 1];

    // rusqlite per-field get calls (handles Option<T> and bool→i32)
    let row_field_gets: Vec<TokenStream2> = input
        .fields
        .iter()
        .zip(row_field_indices)
        .map(|(f, idx)| {
            let n = &f.name;
            let ty = &f.ty;
            if f.optional {
                quote! { #n: row.get::<_, Option<#ty>>(#idx)?, }
            } else {
                quote! { #n: row.get(#idx)?, }
            }
        })
        .collect();

    // Doc strings
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
            "[MUTATES STATE] Register a new {plugin_str} endpoint. Errors if `name` is already taken."
        ),
        Span::call_site(),
    );
    let update_doc = LitStr::new(
        &format!(
            "[MUTATES STATE] Modify an existing {plugin_str} endpoint. PATCH semantics — must already exist."
        ),
        Span::call_site(),
    );
    let delete_doc = LitStr::new(
        &format!("[MUTATES STATE] Remove a registered {plugin_str} endpoint. Idempotent."),
        Span::call_site(),
    );

    let toolkit: Path = syn::parse_quote!(::plugin_toolkit);

    let expanded = quote! {
        // ── Row struct ───────────────────────────────────────────────────
        #[derive(Debug, Clone)]
        pub struct #row_ident {
            pub name: ::std::string::String,
            #( #row_field_decls )*
            pub enabled: bool,
        }

        // ── Schema fragment ──────────────────────────────────────────────
        ::plugin_toolkit::inventory::submit! {
            ::plugin_toolkit::SchemaFragment { name: #table, sql: #create_table_sql }
        }

        // ── DB CRUD module ───────────────────────────────────────────────
        pub mod endpoint_db {
            use super::#row_ident;
            use ::plugin_toolkit::anyhow::Result;
            use ::plugin_toolkit::rusqlite::{Connection, OptionalExtension};

            pub fn list(conn: &Connection) -> Result<::std::vec::Vec<#row_ident>> {
                let mut stmt = conn.prepare(#list_sql)?;
                let rows = stmt.query_map([], |row| {
                    Ok(#row_ident {
                        name: row.get(#row_name_idx)?,
                        #( #row_field_gets )*
                        enabled: row.get::<_, i32>(#row_enabled_idx)? != 0,
                    })
                })?;
                rows.collect::<::plugin_toolkit::rusqlite::Result<::std::vec::Vec<_>>>().map_err(Into::into)
            }

            pub fn get(conn: &Connection, name: &str) -> Result<::std::option::Option<#row_ident>> {
                conn.query_row(
                    #get_sql,
                    ::plugin_toolkit::rusqlite::params![name],
                    |row| Ok(#row_ident {
                        name: row.get(#row_name_idx)?,
                        #( #row_field_gets )*
                        enabled: row.get::<_, i32>(#row_enabled_idx)? != 0,
                    }),
                ).optional().map_err(Into::into)
            }

            pub fn insert(conn: &Connection, ep: &#row_ident) -> Result<()> {
                conn.execute(
                    #insert_sql,
                    ::plugin_toolkit::rusqlite::params![ep.name, #( ep.#field_idents, )* ep.enabled],
                )?;
                Ok(())
            }

            pub fn update(conn: &Connection, ep: &#row_ident) -> Result<bool> {
                let n = conn.execute(
                    #update_sql,
                    ::plugin_toolkit::rusqlite::params![ep.name, #( ep.#field_idents, )* ep.enabled],
                )?;
                Ok(n > 0)
            }

            pub fn upsert(conn: &Connection, ep: &#row_ident) -> Result<()> {
                conn.execute(
                    #upsert_sql,
                    ::plugin_toolkit::rusqlite::params![ep.name, #( ep.#field_idents, )* ep.enabled],
                )?;
                Ok(())
            }

            pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
                let n = conn.execute(
                    #delete_sql,
                    ::plugin_toolkit::rusqlite::params![name],
                )?;
                Ok(n > 0)
            }
        }

        // ── Public-side entry (no secrets) ───────────────────────────────
        #[derive(::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema, Debug, Clone)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        #[serde(rename_all = "camelCase")]
        pub struct #entry_ident {
            pub name: ::std::string::String,
            #( #entry_field_decls )*
            pub enabled: bool,
        }

        // ── list ─────────────────────────────────────────────────────────
        #[derive(::plugin_toolkit::clap::Args, ::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema, Default)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        #[serde(default)]
        pub struct #list_args {}

        #[derive(::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema, Default)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        #[serde(default)]
        pub struct #list_output { pub endpoints: ::std::vec::Vec<#entry_ident> }

        #[doc = #list_doc]
        #[::plugin_toolkit::derive::orca_tool(domain = #plugin_str_lit, verb = "list")]
        async fn #list_fn(_args: #list_args, _ctx: &::plugin_toolkit::contract::ToolCtx) -> ::plugin_toolkit::anyhow::Result<#list_output> {
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
        #[derive(::plugin_toolkit::clap::Args, ::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #detail_args { #[arg(long)] pub name: ::std::string::String }

        #[derive(::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #detail_output { pub endpoint: #entry_ident }

        #[doc = #detail_doc]
        #[::plugin_toolkit::derive::orca_tool(domain = #plugin_str_lit, verb = "detail")]
        async fn #detail_fn(args: #detail_args, _ctx: &::plugin_toolkit::contract::ToolCtx) -> ::plugin_toolkit::anyhow::Result<#detail_output> {
            let conn = #toolkit::runtime::open_db()?;
            let row = endpoint_db::get(&conn, &args.name)?
                .ok_or_else(|| #toolkit::runtime::missing_row_error(#plugin_str_lit, &args.name))?;
            Ok(#detail_output { endpoint: #entry_ident {
                name: row.name.clone(),
                #( #entry_from_row )*
                enabled: row.enabled,
            }})
        }

        // ── create ───────────────────────────────────────────────────────
        #[derive(::plugin_toolkit::clap::Args, ::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #create_args {
            #[arg(long)] pub name: ::std::string::String,
            #( #create_field_decls )*
        }

        #[derive(::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #create_output { pub endpoint: #entry_ident }

        #[doc = #create_doc]
        #[::plugin_toolkit::derive::orca_tool(domain = #plugin_str_lit, verb = "create")]
        async fn #create_fn(args: #create_args, _ctx: &::plugin_toolkit::contract::ToolCtx) -> ::plugin_toolkit::anyhow::Result<#create_output> {
            let row = #row_ident {
                name: args.name.clone(),
                #( #create_row_fields )*
                enabled: true,
            };
            let conn = #toolkit::runtime::open_db()?;
            endpoint_db::insert(&conn, &row)
                .map_err(|e| #toolkit::runtime::map_insert_conflict(e, #plugin_str_lit, &row.name))?;
            Ok(#create_output { endpoint: #entry_ident {
                name: row.name.clone(),
                #( #entry_from_row )*
                enabled: row.enabled,
            }})
        }

        // ── update ───────────────────────────────────────────────────────
        #[derive(::plugin_toolkit::clap::Args, ::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema, Default)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        #[serde(default)]
        pub struct #update_args {
            #[arg(long)] pub name: ::std::string::String,
            #( #update_field_decls )*
            #[arg(long)] pub enabled: Option<bool>,
        }

        #[derive(::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #update_output {
            pub endpoint: #entry_ident,
            pub applied: ::std::vec::Vec<::std::string::String>,
        }

        #[doc = #update_doc]
        #[::plugin_toolkit::derive::orca_tool(domain = #plugin_str_lit, verb = "update")]
        async fn #update_fn(args: #update_args, _ctx: &::plugin_toolkit::contract::ToolCtx) -> ::plugin_toolkit::anyhow::Result<#update_output> {
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
                ::plugin_toolkit::anyhow::bail!("no fields to update; pass at least one flag");
            }
            let changed = endpoint_db::update(&conn, &row)?;
            if !changed { ::plugin_toolkit::anyhow::bail!("update reported no row change for `{}`", row.name); }
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
        #[derive(::plugin_toolkit::clap::Args, ::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #delete_args { #[arg(long)] pub name: ::std::string::String }

        #[derive(::plugin_toolkit::serde::Serialize, ::plugin_toolkit::serde::Deserialize, ::plugin_toolkit::schemars::JsonSchema)]
        #[serde(crate = "::plugin_toolkit::serde")]
        #[schemars(crate = "::plugin_toolkit::schemars")]
        pub struct #delete_output { pub name: ::std::string::String, pub changed: bool }

        #[doc = #delete_doc]
        #[::plugin_toolkit::derive::orca_tool(domain = #plugin_str_lit, verb = "delete")]
        async fn #delete_fn(args: #delete_args, _ctx: &::plugin_toolkit::contract::ToolCtx) -> ::plugin_toolkit::anyhow::Result<#delete_output> {
            let conn = #toolkit::runtime::open_db()?;
            let changed = endpoint_db::remove(&conn, &args.name)?;
            Ok(#delete_output { name: args.name, changed })
        }
    };

    Ok(expanded)
}
