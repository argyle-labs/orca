//! `endpoint_resource!` and `#[endpoint_resource]` — generates the full
//! 5-verb REST surface for an endpoint-registry resource.
//!
//! See [[feedback-plugin-toolkit-max-power-min-boilerplate]].
//!
//! Both forms generate identical output:
//! - `pub struct EndpointRow { name, <fields>, enabled }`
//! - `pub mod endpoint_db { list, get, insert, update, upsert, remove }`
//! - Five `#[orca_tool]` async fns: `<plugin>.{list, detail, create, update, delete}`
//!
//! Table target:
//! - DEFAULT (no explicit `table:`) → SHARED mode: provider-tagged rows in the
//!   ONE core-migrated `endpoints` table; NO per-plugin `SchemaFragment`
//!   (core owns the table), reads scoped to this provider client-side, and
//!   Update/Delete keyed by the minted `id`.
//! - explicit `table:` (e.g. `managed_mounts`) → OWN-TABLE mode: the resource's
//!   own full-spec table + its `SchemaFragment` + `inventory::submit!` and its
//!   optional `lww` replication, keyed by `name`. Unchanged from before.
//!
//! `#[secret]` fields: excluded from `EndpointEntry` (stored only).
//! `Option<T>` + `#[secret]` fields: appear as `has_<name>: bool` in entry.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote};
use syn::{
    Attribute, Ident, LitStr, Token, Type,
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
    /// Opt-in mesh replication: the name of the last-write-wins column (e.g.
    /// `"updated_at"`). When set, the macro adds that column, registers the
    /// table for pod-wide eventually-consistent sync (a `ReplicatedRegistration`
    /// backed by `macro_runtime::replicate_table`), so the row converges
    /// fleet-wide instead of drifting per-host. `None` = local table (today's
    /// behaviour). Only core domain crates set this; thin plugins never do.
    pub(crate) lww: Option<String>,
    /// Shared-table mode (DEFAULT true when no explicit `table:` is given).
    /// In shared mode the generated CRUD writes PROVIDER-TAGGED rows into the
    /// ONE core-migrated `endpoints` table (`table` == `"endpoints"`) instead of
    /// a per-plugin `{plugin}_endpoints` table, and emits NO `SchemaFragment` /
    /// replication registration (core owns `endpoints`). This is the fix for
    /// subprocess plugins whose per-plugin `SchemaFragment` is process-local to
    /// the plugin binary and never reaches the daemon. When `false` (an explicit
    /// `table:` was passed, e.g. `managed_mounts`), the resource keeps its own
    /// full-spec table + `SchemaFragment` + (optional) replication, unchanged.
    pub(crate) shared: bool,
}

impl Parse for EndpointResource {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut plugin: Option<LitStr> = None;
        let mut table: Option<String> = None;
        let mut fields: Option<Vec<EndpointField>> = None;
        let mut lww: Option<String> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            let _: Token![:] = input.parse()?;
            match key.to_string().as_str() {
                "plugin" => plugin = Some(input.parse()?),
                "table" => {
                    let s: LitStr = input.parse()?;
                    table = Some(s.value());
                }
                "lww" => {
                    let s: LitStr = input.parse()?;
                    lww = Some(s.value());
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
                        format!(
                            "unknown key `{other}`; expected one of: plugin, table, lww, fields"
                        ),
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
        // No explicit `table:` → shared mode targeting the core `endpoints` table.
        // An explicit `table:` opts the resource out onto its own full-spec table.
        let shared = table.is_none();
        let table = if shared {
            "endpoints".to_string()
        } else {
            table.expect("explicit table set when !shared")
        };
        Ok(Self {
            plugin,
            table,
            fields,
            shared,
            // Function-macro form (`endpoint_resource! { … }`) doesn't currently
            // accept a `crate = ::path` key — callers are plugin-side and
            // anchor to `::plugin_toolkit` unconditionally. Add a key here when
            // a domain-crate use of the function-form arises.
            crate_path: syn::parse_quote!(::plugin_toolkit),
            lww,
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
    // Shared mode: provider-tagged rows in the ONE core-migrated `endpoints`
    // table. Own-table mode (an explicit `table:`) keeps today's behaviour.
    let shared = input.shared;
    // In shared mode two declared fields, recognised BY NAME, map onto the
    // thin typed columns `endpoints` carries; every OTHER declared field is
    // dropped (not persisted, reconstructed via `Default` on read).
    let has_token_id = input.fields.iter().any(|f| f.name == "token_id");
    let has_insecure = input.fields.iter().any(|f| f.name == "insecure");

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
    // Column names (== field idents) as string literals, for the typed DbRow the
    // generated CRUD builds — every op now runs through core's connection.
    let field_names: Vec<String> = input.fields.iter().map(|f| f.name.to_string()).collect();

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
        .chain(std::iter::once(quote! { routes: row.routes.clone(), }))
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
    // `routes` is a built-in column on every endpoint — the ordered set of
    // reachable paths (FQDN / LAN / Tailscale / …) the resolver falls through.
    // Stored as a JSON array of `plugin_toolkit::route::Route`.
    create_columns.push_str("    routes TEXT NOT NULL DEFAULT '[]',\n");
    create_columns.push_str("    enabled INTEGER NOT NULL DEFAULT 1,\n");
    // A delete is a PHYSICAL removal: the deletion propagates via the
    // command-log (`db::replication_ops`), not an in-row `deleted` column, so
    // there is no tombstone flag on the table itself. See remove() below.
    // `created_at` is the last column unless the macro-managed replication clock
    // column follows.
    let created_at_tail = if input.lww.is_some() { ",\n" } else { "\n" };
    create_columns.push_str(&format!(
        "    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')){created_at_tail}"
    ));
    // Opt-in replication clock: an INTEGER unix-millis column ([[time-values-in-milliseconds]])
    // the macro owns end to end — added here, stamped on every write in
    // `to_dbrow`, and never surfaced as a tool arg. The last-write-wins key the
    // mesh merge compares.
    if let Some(lww) = &input.lww {
        create_columns.push_str(&format!("    {lww} INTEGER NOT NULL DEFAULT 0\n"));
    }
    let create_table_sql = format!("CREATE TABLE IF NOT EXISTS {table} (\n    {create_columns});");

    // The registry table is still created via the `SchemaFragment` inventory
    // (see `create_table_sql` above). All runtime reads/writes now go through
    // core's connection via `endpoint_db`'s typed `DbOp`s — no per-op SQL is
    // generated here anymore.

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

    let crate_path = &input.crate_path;

    // ── Opt-in mesh replication ──────────────────────────────────────────────
    // When `lww` is set, register the table for pod-wide eventually-consistent
    // sync: two free fns delegating to the generic column-list replicator plus a
    // `ReplicatedRegistration` the running engine walks. The column list mirrors
    // the CREATE TABLE order exactly (PK, declared fields, built-ins, lww). When
    // `lww` is None this is empty tokens — a plain local table, unchanged.
    // Token stamping the macro-managed clock column on every write; empty when
    // the table doesn't replicate. Spliced into `to_dbrow` so create/update/
    // upsert all advance the LWW clock without any caller involvement.
    // Canonical unix-millis wall clock ([[time-values-in-milliseconds]]), single-
    // sourced from `utils::time` and re-exported by both macro crate-paths
    // (`macro_runtime` / `plugin_toolkit`) as an always-available light seam.
    // Advances the lww clock to "now" on every live write.
    let now_millis = quote! { #crate_path::now_millis_since_epoch() };
    // On every live write (insert/update/upsert) a replicated row stamps the lww
    // clock. Empty tokens for non-replicated tables.
    let lww_stamp = if let Some(lww) = &input.lww {
        let lww_lit = LitStr::new(lww, Span::call_site());
        quote! {
            m.insert(
                ::std::string::String::from(#lww_lit),
                #crate_path::abi::DbValue::Int(#now_millis),
            );
        }
    } else {
        quote! {}
    };

    // Shared-mode resources never register per-plugin replication — the shared
    // `endpoints` table is registered once in core. Own-table replication is
    // still opt-in via `lww`.
    let replication = if let (false, Some(lww)) = (shared, &input.lww) {
        // The replicated column list mirrors CREATE TABLE order exactly: PK,
        // declared fields, built-ins, then the macro-managed clock column.
        let mut cols: Vec<String> = vec!["name".to_string()];
        cols.extend(input.fields.iter().map(|f| f.name.to_string()));
        cols.push("routes".to_string());
        cols.push("enabled".to_string());
        cols.push("created_at".to_string());
        cols.push(lww.clone());
        let col_lits: Vec<LitStr> = cols
            .iter()
            .map(|c| LitStr::new(c, Span::call_site()))
            .collect();
        let lww_lit = LitStr::new(lww, Span::call_site());
        let table_slug = table.replace('-', "_");
        let export_fn = format_ident!("__replicate_export_{}", table_slug);
        let merge_fn = format_ident!("__replicate_merge_{}", table_slug);
        quote! {
            // Free-form JSON at the replication boundary is intentional (the
            // bundle is heterogeneous per-table rows) — same justified allow the
            // `Replicated` derive carries on its generated impl.
            #[allow(clippy::disallowed_types)]
            fn #export_fn(
                conn: &#crate_path::rusqlite::Connection,
            ) -> #crate_path::anyhow::Result<#crate_path::serde_json::Value> {
                #crate_path::replicate_table::export_table(conn, #table, &[#(#col_lits),*], "name")
            }
            #[allow(clippy::disallowed_types)]
            fn #merge_fn(
                conn: &#crate_path::rusqlite::Connection,
                rows: #crate_path::serde_json::Value,
            ) -> #crate_path::anyhow::Result<usize> {
                #crate_path::replicate_table::merge_table(
                    conn, #table, &[#(#col_lits),*], "name", #lww_lit, rows,
                )
            }
            #crate_path::inventory::submit! {
                #crate_path::ReplicatedRegistration {
                    name: #table,
                    export: #export_fn,
                    merge: #merge_fn,
                }
            }
        }
    } else {
        quote! {}
    };

    // ── Schema fragment (own-table mode only) ─────────────────────────────────
    // Shared-mode resources emit NO fragment: the daemon owns `endpoints` via
    // `apply_schema`. That is the whole fix — a per-plugin fragment is
    // process-local to a subprocess plugin binary and never reaches core.
    let schema_fragment = if shared {
        quote! {}
    } else {
        quote! {
            #crate_path::inventory::submit! {
                #crate_path::SchemaFragment { name: #table, sql: #create_table_sql }
            }
        }
    };

    // ── The `require` helper (identical in both modes) ────────────────────────
    let require_fn = quote! {
        /// Resolve a registered, enabled endpoint by name — the standard
        /// preamble every tool/client helper repeats. Errors if the endpoint
        /// is not registered or is disabled, so callers get the row directly.
        pub fn require(name: &str) -> Result<#row_ident> {
            use #crate_path::anyhow::{anyhow, bail};
            let row = get(name)?
                .ok_or_else(|| anyhow!(concat!(#plugin_str_lit, " endpoint '{}' not registered"), name))?;
            if !row.enabled {
                bail!(concat!(#plugin_str_lit, " endpoint '{}' is disabled"), name);
            }
            Ok(row)
        }
    };

    // ── endpoint_db module ────────────────────────────────────────────────────
    // Two shapes. Own-table mode keeps today's behaviour verbatim (keyed by
    // `name` on the plugin's own table). Shared mode writes PROVIDER-TAGGED rows
    // into `endpoints`, scopes reads to this provider client-side, and keys
    // Update/Delete by the row's minted `id`.
    let endpoint_db_mod = if !shared {
        quote! {
            pub mod endpoint_db {
                use super::#row_ident;
                use #crate_path::anyhow::Result;
                use #crate_path::abi::{DbOp, DbRow, DbValue};
                use #crate_path::runtime::{db_op, field_from_row, ToDbValue};

                const TABLE: &str = #table;

                fn to_dbrow(ep: &#row_ident) -> DbRow {
                    let mut m = DbRow::new();
                    m.insert(::std::string::String::from("name"), DbValue::Text(ep.name.clone()));
                    #( m.insert(
                        ::std::string::String::from(#field_names),
                        ToDbValue::to_dbvalue(&ep.#field_idents),
                    ); )*
                    m.insert(
                        ::std::string::String::from("routes"),
                        DbValue::Text(
                            #crate_path::serde_json::to_string(&ep.routes)
                                .unwrap_or_else(|_| ::std::string::String::from("[]")),
                        ),
                    );
                    m.insert(::std::string::String::from("enabled"), DbValue::Bool(ep.enabled));
                    #lww_stamp
                    m
                }

                fn from_dbrow(m: &DbRow) -> Result<#row_ident> {
                    Ok(#row_ident {
                        name: field_from_row(m, "name")?,
                        #( #field_idents: field_from_row(m, #field_names)?, )*
                        routes: {
                            let __json: ::std::string::String = field_from_row(m, "routes")?;
                            #crate_path::serde_json::from_str(&__json).unwrap_or_default()
                        },
                        enabled: field_from_row::<bool>(m, "enabled")?,
                    })
                }

                pub fn list() -> Result<::std::vec::Vec<#row_ident>> {
                    let reply = db_op(&DbOp::List {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                    })?;
                    reply.rows.iter().map(from_dbrow).collect()
                }

                pub fn get(name: &str) -> Result<::std::option::Option<#row_ident>> {
                    let reply = db_op(&DbOp::Get {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        key_col: ::std::string::String::from("name"),
                        key: ::std::string::String::from(name),
                    })?;
                    match reply.rows.first() {
                        ::std::option::Option::Some(r) => Ok(::std::option::Option::Some(from_dbrow(r)?)),
                        ::std::option::Option::None => Ok(::std::option::Option::None),
                    }
                }

                #require_fn

                pub fn insert(ep: &#row_ident) -> Result<()> {
                    db_op(&DbOp::Insert {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        row: to_dbrow(ep),
                    })?;
                    Ok(())
                }

                pub fn update(ep: &#row_ident) -> Result<bool> {
                    let reply = db_op(&DbOp::Update {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        key_col: ::std::string::String::from("name"),
                        row: to_dbrow(ep),
                    })?;
                    Ok(reply.affected > 0)
                }

                pub fn upsert(ep: &#row_ident) -> Result<()> {
                    db_op(&DbOp::Upsert {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        row: to_dbrow(ep),
                    })?;
                    Ok(())
                }

                pub fn remove(name: &str) -> Result<bool> {
                    let reply = db_op(&DbOp::Delete {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        key_col: ::std::string::String::from("name"),
                        key: ::std::string::String::from(name),
                    })?;
                    Ok(reply.affected > 0)
                }
            }
        }
    } else {
        // Shared mode. `token_id` (if declared) ↔ `auth_principal`; `insecure`
        // (if declared) ↔ `insecure`; every other declared field is dropped.
        let auth_principal_write = if has_token_id {
            quote! { ToDbValue::to_dbvalue(&ep.token_id) }
        } else {
            quote! { DbValue::Null }
        };
        let insecure_write = if has_insecure {
            quote! { ToDbValue::to_dbvalue(&ep.insecure) }
        } else {
            quote! { DbValue::Null }
        };
        // from_dbrow field reconstruction for the FULL EndpointRow.
        let from_row_fields: Vec<TokenStream2> = input
            .fields
            .iter()
            .map(|f| {
                let n = &f.name;
                if n == "token_id" {
                    quote! { #n: field_from_row(m, "auth_principal")?, }
                } else if n == "insecure" {
                    quote! { #n: field_from_row(m, "insecure")?, }
                } else {
                    // Dropped field — not persisted in the thin shared table.
                    quote! { #n: ::std::default::Default::default(), }
                }
            })
            .collect();
        quote! {
            pub mod endpoint_db {
                use super::#row_ident;
                use #crate_path::anyhow::Result;
                use #crate_path::abi::{DbOp, DbRow, DbValue};
                use #crate_path::runtime::{db_op, field_from_row, ToDbValue};

                const TABLE: &str = "endpoints";
                const PROVIDER: &str = #plugin_str_lit;

                // Build the thin, provider-tagged `endpoints` row. `id` is the
                // minted uuidv7 PK; `updated_at` is the LWW clock, stamped on
                // every live write. Secrets are never included here.
                fn to_dbrow(ep: &#row_ident, id: &str) -> DbRow {
                    let mut m = DbRow::new();
                    m.insert(::std::string::String::from("id"), DbValue::Text(::std::string::String::from(id)));
                    m.insert(::std::string::String::from("provider"), DbValue::Text(::std::string::String::from(PROVIDER)));
                    m.insert(::std::string::String::from("name"), DbValue::Text(ep.name.clone()));
                    m.insert(
                        ::std::string::String::from("routes"),
                        DbValue::Text(
                            #crate_path::serde_json::to_string(&ep.routes)
                                .unwrap_or_else(|_| ::std::string::String::from("[]")),
                        ),
                    );
                    m.insert(::std::string::String::from("enabled"), DbValue::Bool(ep.enabled));
                    m.insert(::std::string::String::from("auth_principal"), #auth_principal_write);
                    m.insert(::std::string::String::from("insecure"), #insecure_write);
                    m.insert(
                        ::std::string::String::from("updated_at"),
                        DbValue::Int(#crate_path::now_millis_since_epoch()),
                    );
                    m
                }

                fn from_dbrow(m: &DbRow) -> Result<#row_ident> {
                    Ok(#row_ident {
                        name: field_from_row(m, "name")?,
                        #( #from_row_fields )*
                        routes: {
                            let __json: ::std::string::String = field_from_row(m, "routes")?;
                            #crate_path::serde_json::from_str(&__json).unwrap_or_default()
                        },
                        enabled: field_from_row::<bool>(m, "enabled")?,
                    })
                }

                // True when a returned `endpoints` row belongs to THIS provider —
                // the client-side scoping that lets every plugin share the table.
                fn is_ours(r: &DbRow) -> bool {
                    field_from_row::<::std::string::String>(r, "provider")
                        .map(|p| p == PROVIDER)
                        .unwrap_or(false)
                }

                // Resolve the minted `id` PK for a (provider, name) pair.
                fn resolve_id(name: &str) -> Result<::std::option::Option<::std::string::String>> {
                    let reply = db_op(&DbOp::List {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                    })?;
                    for r in &reply.rows {
                        if is_ours(r)
                            && field_from_row::<::std::string::String>(r, "name")? == name
                        {
                            return Ok(::std::option::Option::Some(field_from_row(r, "id")?));
                        }
                    }
                    Ok(::std::option::Option::None)
                }

                pub fn list() -> Result<::std::vec::Vec<#row_ident>> {
                    let reply = db_op(&DbOp::List {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                    })?;
                    reply.rows.iter().filter(|r| is_ours(r)).map(from_dbrow).collect()
                }

                pub fn get(name: &str) -> Result<::std::option::Option<#row_ident>> {
                    let reply = db_op(&DbOp::List {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                    })?;
                    for r in &reply.rows {
                        if is_ours(r)
                            && field_from_row::<::std::string::String>(r, "name")? == name
                        {
                            return Ok(::std::option::Option::Some(from_dbrow(r)?));
                        }
                    }
                    Ok(::std::option::Option::None)
                }

                #require_fn

                pub fn insert(ep: &#row_ident) -> Result<()> {
                    let id = #crate_path::mint_uuidv7();
                    db_op(&DbOp::Insert {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        row: to_dbrow(ep, &id),
                    })?;
                    Ok(())
                }

                pub fn update(ep: &#row_ident) -> Result<bool> {
                    let id = match resolve_id(&ep.name)? {
                        ::std::option::Option::Some(id) => id,
                        ::std::option::Option::None => return Ok(false),
                    };
                    let reply = db_op(&DbOp::Update {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        key_col: ::std::string::String::from("id"),
                        row: to_dbrow(ep, &id),
                    })?;
                    Ok(reply.affected > 0)
                }

                pub fn upsert(ep: &#row_ident) -> Result<()> {
                    if get(&ep.name)?.is_some() {
                        update(ep)?;
                    } else {
                        insert(ep)?;
                    }
                    Ok(())
                }

                pub fn remove(name: &str) -> Result<bool> {
                    let id = match resolve_id(name)? {
                        ::std::option::Option::Some(id) => id,
                        ::std::option::Option::None => return Ok(false),
                    };
                    let reply = db_op(&DbOp::Delete {
                        namespace: ::std::string::String::new(),
                        table: ::std::string::String::from(TABLE),
                        key_col: ::std::string::String::from("id"),
                        key: id,
                    })?;
                    Ok(reply.affected > 0)
                }
            }
        }
    };

    // `#[serde(crate = "...")]` / `#[schemars(crate = "...")]` take a string
    // literal, so stringify the path tokens once and reuse.
    let crate_path_str = crate_path.to_token_stream().to_string().replace(' ', "");
    let serde_path_str = format!("{crate_path_str}::serde");
    let schemars_path_str = format!("{crate_path_str}::schemars");

    let expanded = quote! {
        // ── Row struct ───────────────────────────────────────────────────
        #[derive(Debug, Clone)]
        pub struct #row_ident {
            pub name: ::std::string::String,
            #( #row_field_decls )*
            pub routes: #crate_path::route::Routes,
            pub enabled: bool,
        }

        // ── Schema fragment ──────────────────────────────────────────────
        // Own-table mode only. In shared mode the daemon owns the `endpoints`
        // table via `apply_schema` (empty tokens here), which is the whole fix
        // for subprocess plugins whose per-plugin fragment never reaches core.
        #schema_fragment

        // ── Opt-in mesh replication registration ──────────────────────────
        // Own-table mode only (empty unless `lww` is set). In shared mode the
        // `endpoints` table's replication is registered ONCE in core, not per
        // plugin — so this is empty tokens for a shared resource.
        #replication

        // ── DB CRUD module ───────────────────────────────────────────────
        // Every op runs through core's single pooled connection via
        // `runtime::db_op` (typed [`DbOp`]). The plugin NEVER opens its own
        // connection — that second connection raced the daemon's on the WAL/shm
        // index (SQLITE_IOERR_SHMOPEN 5898). The registry table is core-migrated
        // and owned by name, so ops carry an empty namespace + the literal table.
        #endpoint_db_mod

        // ── Public-side entry (no secrets) ───────────────────────────────
        #[derive(#crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema, Debug, Clone)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        #[serde(rename_all = "camelCase")]
        pub struct #entry_ident {
            pub name: ::std::string::String,
            #( #entry_field_decls )*
            pub routes: #crate_path::route::Routes,
            pub enabled: bool,
        }

        // ── list ─────────────────────────────────────────────────────────
        #[derive(#crate_path::clap::Args, #crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema, Default)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        #[serde(default)]
        pub struct #list_args {}

        #[derive(#crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema, Default)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        #[serde(default)]
        pub struct #list_output { pub endpoints: ::std::vec::Vec<#entry_ident> }

        #[doc = #list_doc]
        #[#crate_path::derive::orca_tool(domain = #plugin_str_lit, verb = "list")]
        async fn #list_fn(_args: #list_args, _ctx: &#crate_path::contract::ToolCtx) -> #crate_path::anyhow::Result<#list_output> {
            let endpoints = endpoint_db::list()?
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
        #[derive(#crate_path::clap::Args, #crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #detail_args { #[arg(long)] pub name: ::std::string::String }

        #[derive(#crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #detail_output { pub endpoint: #entry_ident }

        #[doc = #detail_doc]
        #[#crate_path::derive::orca_tool(domain = #plugin_str_lit, verb = "detail")]
        async fn #detail_fn(args: #detail_args, _ctx: &#crate_path::contract::ToolCtx) -> #crate_path::anyhow::Result<#detail_output> {
            let row = endpoint_db::get(&args.name)?
                .ok_or_else(|| #crate_path::runtime::missing_row_error(#plugin_str_lit, &args.name))?;
            Ok(#detail_output { endpoint: #entry_ident {
                name: row.name.clone(),
                #( #entry_from_row )*
                enabled: row.enabled,
            }})
        }

        // ── create ───────────────────────────────────────────────────────
        #[derive(#crate_path::clap::Args, #crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #create_args {
            #[arg(long)] pub name: ::std::string::String,
            #( #create_field_decls )*
            /// Reachable path(s), tried in order. Repeatable: `--route kind=url`
            /// or a JSON object. e.g. `--route lan=http://10.0.0.5:8989`.
            // Bare `Vec` + explicit `Append`: clap's derive only recognises a
            // multi-value arg from a literal `Vec<…>` field type, and a
            // fully-qualified `::std::vec::Vec` silently degrades it to a scalar.
            #[arg(
                long = "route",
                value_parser = #crate_path::route::parse_route,
                action = #crate_path::clap::ArgAction::Append,
            )]
            #[serde(default)]
            pub routes: Vec<#crate_path::route::Route>,
        }

        #[derive(#crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #create_output { pub endpoint: #entry_ident }

        #[doc = #create_doc]
        #[#crate_path::derive::orca_tool(domain = #plugin_str_lit, verb = "create")]
        async fn #create_fn(args: #create_args, _ctx: &#crate_path::contract::ToolCtx) -> #crate_path::anyhow::Result<#create_output> {
            let row = #row_ident {
                name: args.name.clone(),
                #( #create_row_fields )*
                routes: #crate_path::route::Routes::from(args.routes),
                enabled: true,
            };
            endpoint_db::insert(&row)
                .map_err(|e| #crate_path::runtime::map_insert_conflict(e, #plugin_str_lit, &row.name))?;
            Ok(#create_output { endpoint: #entry_ident {
                name: row.name.clone(),
                #( #entry_from_row )*
                enabled: row.enabled,
            }})
        }

        // ── update ───────────────────────────────────────────────────────
        #[derive(#crate_path::clap::Args, #crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema, Default)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        #[serde(default)]
        pub struct #update_args {
            #[arg(long)] pub name: ::std::string::String,
            #( #update_field_decls )*
            /// Replace the reachable-path set. Repeatable: `--route kind=url`
            /// or a JSON object. Omit to leave routes unchanged.
            #[arg(
                long = "route",
                value_parser = #crate_path::route::parse_route,
                action = #crate_path::clap::ArgAction::Append,
            )]
            #[serde(default)]
            pub routes: Vec<#crate_path::route::Route>,
            #[arg(long)] pub enabled: Option<bool>,
        }

        #[derive(#crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #update_output {
            pub endpoint: #entry_ident,
            pub applied: ::std::vec::Vec<::std::string::String>,
        }

        #[doc = #update_doc]
        #[#crate_path::derive::orca_tool(domain = #plugin_str_lit, verb = "update")]
        async fn #update_fn(args: #update_args, _ctx: &#crate_path::contract::ToolCtx) -> #crate_path::anyhow::Result<#update_output> {
            let mut row = endpoint_db::get(&args.name)?
                .ok_or_else(|| #crate_path::runtime::missing_row_error(#plugin_str_lit, &args.name))?;
            let mut applied: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
            #( #update_patch_stanzas )*
            if !args.routes.is_empty() {
                row.routes = #crate_path::route::Routes::from(args.routes);
                applied.push("routes".to_string());
            }
            if let ::std::option::Option::Some(v) = args.enabled {
                row.enabled = v;
                applied.push("enabled".to_string());
            }
            if applied.is_empty() {
                #crate_path::anyhow::bail!("no fields to update; pass at least one flag");
            }
            let changed = endpoint_db::update(&row)?;
            if !changed { #crate_path::anyhow::bail!("update reported no row change for `{}`", row.name); }
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
        #[derive(#crate_path::clap::Args, #crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #delete_args { #[arg(long)] pub name: ::std::string::String }

        #[derive(#crate_path::serde::Serialize, #crate_path::serde::Deserialize, #crate_path::schemars::JsonSchema)]
        #[serde(crate = #serde_path_str)]
        #[schemars(crate = #schemars_path_str)]
        pub struct #delete_output { pub name: ::std::string::String, pub changed: bool }

        #[doc = #delete_doc]
        #[#crate_path::derive::orca_tool(domain = #plugin_str_lit, verb = "delete")]
        async fn #delete_fn(args: #delete_args, _ctx: &#crate_path::contract::ToolCtx) -> #crate_path::anyhow::Result<#delete_output> {
            let changed = endpoint_db::remove(&args.name)?;
            Ok(#delete_output { name: args.name, changed })
        }
    };

    Ok(expanded)
}
