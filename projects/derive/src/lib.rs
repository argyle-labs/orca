//! `derive` — proc-macro crate. Paired with the `dispatch` runtime crate
//! (macro+runtime split forced by Rust's proc-macro crate restrictions, like
//! `serde-derive`+`serde`). Emits inventory entries at compile time that
//! `dispatch` walks at startup. Hosts `#[orca_tool]` and `#[derive(Replicated)]`.
//! NOT mesh-dispatch — peer calls live in `pod`.
//!
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
//!   - `inventory::submit!` into the `ToolRegistration` slice exposed by
//!     `orca-dispatch` so the dispatchers pick it up at startup without any
//!     central enrollment list.
//!   - An `OpenApiToolRegistration` inventory entry — the spec endpoint hoists
//!     every tool path automatically (see `orca-dispatch::openapi`).
//!
//! Scope: this slice only supports the canonical `async fn name(args: T,
//! ctx: &ToolCtx) -> Result<O>` form. Named-parameter expansion can be added
//! later by destructuring `args` inside the thunk.
//!
//! Scope note: this proc-macro emits paths into `orca-contract` (cold
//! types + trait anchors) and `orca-dispatch` (runtime). It never refers
//! back to the prior `orca-tool` crate, which has been dissolved into
//! those two.

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
#[cfg(not(test))]
use syn::{Data, DeriveInput, Fields};

#[cfg(not(test))]
mod endpoint_resource;
#[cfg(not(test))]
mod endpoint_resource_attr;
mod endpoint_tool;
mod orca_async;
#[cfg(not(test))]
mod plugin_error;
#[cfg(not(test))]
mod plugin_struct;

/// `#[orca_async]` — orca's native sugar for async traits.
///
/// The one attribute a plugin (or core) writes to make an async trait or impl
/// `dyn`-compatible: annotate it, write plain `async fn` methods, and orca owns
/// the boxing, pinning, lifetimes, and runtime behind it. Used on the core
/// domain traits (`StorageBackend`, `RuntimeAdapter`, notifications `Backend`,
/// …) and every plugin's impl of them.
///
/// ```rust,ignore
/// #[orca_async]
/// pub trait StorageBackend: Send + Sync {
///     async fn mount(&self, id: &str) -> Result<MountOutcome, StorageError>;
/// }
///
/// #[orca_async]
/// impl StorageBackend for NfsBackend {
///     async fn mount(&self, id: &str) -> Result<MountOutcome, StorageError> { /* just await */ }
/// }
/// ```
#[cfg(not(test))]
#[proc_macro_attribute]
pub fn orca_async(_attr: TokenStream, item: TokenStream) -> TokenStream {
    orca_async::expand(item.into()).into()
}

/// `#[endpoint_tool(domain = "...", verb = "...")]` — sugar for the ubiquitous
/// "resolve a registered endpoint's client, call it, return the result" tool.
///
/// The author writes only the endpoint call; the macro generates the args
/// struct (endpoint name + the fn's remaining params) and the `#[orca_tool]`
/// wrapper that resolves the client (via an in-scope `make_client`, or
/// `resolve = <path>`) and runs the body. See `endpoint_tool.rs`.
///
/// ```rust,ignore
/// #[endpoint_tool(domain = "home-assistant", verb = "entities")]
/// async fn ha_entities(client: Client, #[arg(long)] domain: Option<String>) -> Result<JsonAny> {
///     Ok(client.entity_list(domain.as_deref()).await?.into())
/// }
/// ```
#[cfg(not(test))]
#[proc_macro_attribute]
pub fn endpoint_tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    endpoint_tool::expand(attr.into(), item.into()).into()
}

/// `#[endpoint_resource(plugin = "...")]` — annotate a struct to generate the
/// full 5-verb endpoint-registry surface with zero SQL in the plugin.
///
/// ```rust,ignore
/// #[endpoint_resource(plugin = "ntfy")]
/// pub struct NtfyEndpoint {
///     pub name: String,
///     pub base_url: String,
///     pub topic: String,
///     #[secret]
///     pub token: Option<String>,
///     pub enabled: bool,
/// }
/// ```
#[cfg(not(test))]
#[proc_macro_attribute]
pub fn endpoint_resource(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as endpoint_resource_attr::EndpointResourceAttr);
    let item = parse_macro_input!(item as syn::ItemStruct);
    match endpoint_resource_attr::expand(attr, item) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// `#[plugin_struct]` — inject the standard plugin-author derive set with
/// crate paths anchored to `#crate_path::*`, so plugins do not need
/// direct deps on serde/schemars/clap. See `plugin_struct.rs`.
#[cfg(not(test))]
#[proc_macro_attribute]
pub fn plugin_struct(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(attr as plugin_struct::PluginStructAttr);
    let item = parse_macro_input!(item as syn::DeriveInput);
    plugin_struct::expand(attr, item).into()
}

/// `#[plugin_error]` — the orca-native error abstraction. Applied to an enum
/// whose variants carry `#[plugin(display = "...")]` (and optional
/// `#[plugin(from)]`), it emits `Display` + `std::error::Error` (+ `From`)
/// without the plugin ever naming `thiserror`. See `plugin_error.rs`.
#[cfg(not(test))]
#[proc_macro_attribute]
pub fn plugin_error(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as syn::DeriveInput);
    plugin_error::expand(item).into()
}

/// Parsed contents of `#[orca_tool(domain = "...", verb = "...", cli = ident)]`.
struct ToolAttr {
    domain: LitStr,
    verb: LitStr,
    cli_mode: Option<Ident>,
    /// Whether this tool is callable by paired pod peers via `pod/exec`.
    /// **Default: true.** Set `local_only = true` (or `remote_ok = false`)
    /// for tools that genuinely can't run remotely — e.g. bootstrap, daemon
    /// install/uninstall, package build. The dispatcher additionally requires
    /// admin auth on every remote invocation regardless of this flag.
    remote_ok: bool,
    /// Opt-in: `#[orca_tool(..., refresh_runtime = true)]` schedules a
    /// best-effort `RemoteExec::refresh_peer_runtime(peer)` after a successful
    /// peer dispatch. Use for tools whose success mutates the peer's reported
    /// runtime snapshot (version, channel, mode) — `system.update` is the
    /// canonical case. Default off so secret/config writes don't pay for it.
    /// Meaningless on `local_only` tools (compile-time rejected).
    refresh_runtime: bool,
    /// Opt-in: `#[orca_tool(..., data_mutation = true)]` marks this tool a
    /// **data mutation** (a write against an external managed system). Data
    /// mutations default to `role = "admin"` but become invokable by a
    /// non-admin identity that holds the `can_mutate` opt-in capability. Set by
    /// the surface generators on mutating operations; control-plane admin tools
    /// leave it off so the opt-in can't reach them. Default off.
    data_mutation: bool,
    /// Minimum role required to invoke this tool via authenticated surfaces.
    /// `"any"` (default) means any authenticated identity passes; `"admin"`
    /// requires `AuthIdentity::role == "admin"`. Set via
    /// `#[orca_tool(..., role = "admin")]`.
    role: Option<LitStr>,
    /// Short human-friendly title shown in the API reference left nav
    /// (Scalar `summary` field). Doc comment is reserved for the full
    /// markdown description body. Set via `#[orca_tool(..., title = "...")]`;
    /// when absent, the canonical tool name (`<domain>.<verb>`) is used.
    title: Option<LitStr>,
    /// Crate path the macro emits against. Defaults to `::plugin_toolkit`
    /// (plugin-author surface). Domain crates that live underneath
    /// `plugin-toolkit` (and therefore can't depend on it without a Cargo
    /// cycle) pass `crate = ::macro_runtime` to anchor emissions against
    /// the lower-level macro-runtime crate, which re-exports the same set
    /// of macro-target dependencies.
    crate_path: syn::Path,
}

impl Parse for ToolAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let items = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        let mut domain = None;
        let mut verb = None;
        let mut cli_mode = None;
        let mut remote_ok = true;
        let mut refresh_runtime = false;
        let mut data_mutation = false;
        let mut role: Option<LitStr> = None;
        let mut title: Option<LitStr> = None;
        let mut crate_path: Option<syn::Path> = None;
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
                "local_only" => {
                    // Inverse opt-out for the remote_ok=true default. Reads
                    // more naturally at the call site for tools that genuinely
                    // can't run remotely (bootstrap, daemon install, etc.).
                    let v = match &nv.value {
                        Expr::Lit(ExprLit {
                            lit: Lit::Bool(b), ..
                        }) => b.value,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "local_only expects a bool literal",
                            ));
                        }
                    };
                    if v {
                        remote_ok = false;
                    }
                }
                "refresh_runtime" => {
                    refresh_runtime = match &nv.value {
                        Expr::Lit(ExprLit {
                            lit: Lit::Bool(b), ..
                        }) => b.value,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "refresh_runtime expects a bool literal",
                            ));
                        }
                    };
                }
                "data_mutation" => {
                    data_mutation = match &nv.value {
                        Expr::Lit(ExprLit {
                            lit: Lit::Bool(b), ..
                        }) => b.value,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "data_mutation expects a bool literal",
                            ));
                        }
                    };
                }
                "role" => {
                    let s = lit_str(&nv.value)?;
                    match s.value().as_str() {
                        "any" | "read" | "admin" => {}
                        other => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                format!(
                                    "role must be \"any\", \"read\", or \"admin\", got {other:?}"
                                ),
                            ));
                        }
                    }
                    role = Some(s);
                }
                "title" => {
                    title = Some(lit_str(&nv.value)?);
                }
                "crate" => {
                    crate_path = Some(parse_path(&nv.value)?);
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
            refresh_runtime,
            data_mutation,
            role,
            title,
            crate_path: crate_path.unwrap_or_else(|| syn::parse_quote!(::plugin_toolkit)),
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

/// Parse a path-valued attribute value like `crate = ::plugin_toolkit`.
/// The expression form accepted is `syn::Expr::Path` — anything else (string
/// literals, integers, calls) is rejected with a clear message.
fn parse_path(expr: &Expr) -> syn::Result<syn::Path> {
    match expr {
        Expr::Path(p) => Ok(p.path.clone()),
        _ => Err(syn::Error::new_spanned(
            expr,
            "expected a path (e.g. `::plugin_toolkit` or `::macro_runtime`)",
        )),
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

/// `#[derive(Replicated)]` — opt a row struct into mesh replication.
///
/// ```rust,ignore
/// #[derive(Serialize, Deserialize, Replicated)]
/// #[replicate(table = "users", lww = "updated_at")]   // pk defaults to "id"
/// pub struct ReplicaUser { pub id: String, /* … one field per column … */ }
/// ```
///
/// Generates `export`/`merge` fns over the named struct fields (each field maps
/// 1:1 to a column of `table`, in declaration order) and submits a
/// `#crate_path::db_types::ReplicatedRegistration` into the inventory slice the pod mesh
/// engine walks. Merge is last-write-wins on the `lww` column, keyed by `pk`.
#[cfg(not(test))]
#[proc_macro_derive(Replicated, attributes(replicate))]
pub fn derive_replicated(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    expand_replicated_to_tokens(input).into()
}

#[cfg(not(test))]
fn expand_replicated_to_tokens(input: DeriveInput) -> TokenStream2 {
    match expand_replicated(input) {
        Ok(ts) => ts,
        Err(e) => e.to_compile_error(),
    }
}

/// Parsed `#[replicate(table = "...", lww = "...", pk = "...", unique = "...")]`.
#[cfg(not(test))]
struct ReplicateAttr {
    table: String,
    lww: String,
    pk: String,
    /// Optional secondary UNIQUE column that acts as a NATURAL identity key
    /// (e.g. `users.username_lower`). The mesh assigns each row a host-local
    /// primary key at bootstrap, so two hosts can hold the SAME logical entity
    /// under DIFFERENT `pk` values but identical `unique` values. Without this,
    /// merging the peer's row does a plain INSERT that trips the secondary
    /// UNIQUE constraint on every tick (the users-merge flood). When set, the
    /// merge treats a `unique` collision as a last-write-wins UPDATE of the
    /// EXISTING local row, PRESERVING the local `pk` (so FK references stay
    /// intact) — the interim step until canonical uuidv7 identity converges
    /// the ids fleet-wide.
    unique: Option<String>,
    /// Crate path the macro emits against. Defaults to `::plugin_toolkit`;
    /// domain crates pass `crate = ::macro_runtime` to anchor against the
    /// lower-level macro-runtime crate (breaks the Cargo cycle that would
    /// otherwise exist if a domain crate hosted under `plugin-toolkit` tried
    /// to depend on `plugin-toolkit`).
    crate_path: syn::Path,
}

#[cfg(not(test))]
fn parse_replicate_attr(attrs: &[Attribute]) -> syn::Result<ReplicateAttr> {
    let attr = attrs
        .iter()
        .find(|a| a.path().is_ident("replicate"))
        .ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "#[derive(Replicated)] requires a #[replicate(table = \"...\", lww = \"...\")] attribute",
            )
        })?;
    let items = attr.parse_args_with(Punctuated::<MetaNameValue, Token![,]>::parse_terminated)?;
    let mut table = None;
    let mut lww = None;
    let mut pk = None;
    let mut unique = None;
    let mut crate_path: Option<syn::Path> = None;
    for nv in items {
        let key = nv
            .path
            .get_ident()
            .map(|i| i.to_string())
            .unwrap_or_default();
        if key == "crate" {
            crate_path = Some(parse_path(&nv.value)?);
            continue;
        }
        let val = match &nv.value {
            Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) => s.value(),
            _ => {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "expected a string literal",
                ));
            }
        };
        match key.as_str() {
            "table" => table = Some(val),
            "lww" => lww = Some(val),
            "pk" => pk = Some(val),
            "unique" => unique = Some(val),
            other => {
                return Err(syn::Error::new_spanned(
                    &nv.path,
                    format!("unknown #[replicate] key '{other}'"),
                ));
            }
        }
    }
    Ok(ReplicateAttr {
        table: table
            .ok_or_else(|| syn::Error::new(Span::call_site(), "#[replicate] requires `table`"))?,
        lww: lww
            .ok_or_else(|| syn::Error::new(Span::call_site(), "#[replicate] requires `lww`"))?,
        pk: pk.unwrap_or_else(|| "id".to_string()),
        unique,
        crate_path: crate_path.unwrap_or_else(|| syn::parse_quote!(::plugin_toolkit)),
    })
}

#[cfg(not(test))]
fn expand_replicated(input: DeriveInput) -> syn::Result<TokenStream2> {
    let cfg = parse_replicate_attr(&input.attrs)?;
    let crate_path = &cfg.crate_path;
    let ty = &input.ident;

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => &n.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    ty,
                    "#[derive(Replicated)] requires a struct with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                ty,
                "#[derive(Replicated)] can only be applied to structs",
            ));
        }
    };

    let field_idents: Vec<&Ident> = named
        .iter()
        .map(|f| f.ident.as_ref().expect("named field"))
        .collect();
    let field_names: Vec<String> = field_idents.iter().map(|i| i.to_string()).collect();

    if !field_names.contains(&cfg.pk) {
        return Err(syn::Error::new_spanned(
            ty,
            format!("#[replicate] pk '{}' is not a field of the struct", cfg.pk),
        ));
    }
    if !field_names.contains(&cfg.lww) {
        return Err(syn::Error::new_spanned(
            ty,
            format!(
                "#[replicate] lww '{}' is not a field of the struct",
                cfg.lww
            ),
        ));
    }
    if let Some(u) = &cfg.unique
        && !field_names.contains(u)
    {
        return Err(syn::Error::new_spanned(
            ty,
            format!("#[replicate] unique '{u}' is not a field of the struct"),
        ));
    }

    let table = &cfg.table;
    let pk = &cfg.pk;
    let lww_ident = Ident::new(&cfg.lww, Span::call_site());

    // SELECT col0, col1, … FROM table ORDER BY pk ASC
    let columns_csv = field_names.join(", ");
    let select_sql = format!("SELECT {columns_csv} FROM {table} ORDER BY {pk} ASC");

    // Row construction in the query_map closure: Self { f0: r.get(0)?, … }.
    let get_indices: Vec<syn::Index> = (0..field_idents.len()).map(syn::Index::from).collect();

    // INSERT … VALUES (?1, …) ON CONFLICT(pk) DO UPDATE SET <non-pk cols>.
    let placeholders = (1..=field_idents.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let update_set = field_names
        .iter()
        .filter(|f| **f != cfg.pk)
        .map(|f| format!("{f} = excluded.{f}"))
        .collect::<Vec<_>>()
        .join(", ");
    // When a NATURAL `unique` key is declared, a peer's row may collide on that
    // column while carrying a DIFFERENT pk (each host mints its own pk at
    // bootstrap). Add a second ON CONFLICT clause so the collision resolves as
    // an UPDATE of the existing local row rather than a fatal INSERT. On the
    // unique-conflict path we PRESERVE the local pk (never SET it) so FK
    // references stay intact, and we don't touch the unique column itself.
    let conflict_clause = match &cfg.unique {
        None => format!("ON CONFLICT({pk}) DO UPDATE SET {update_set}"),
        Some(unique) => {
            let update_set_unique = field_names
                .iter()
                .filter(|f| **f != cfg.pk && **f != *unique)
                .map(|f| format!("{f} = excluded.{f}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "ON CONFLICT({pk}) DO UPDATE SET {update_set} \
                 ON CONFLICT({unique}) DO UPDATE SET {update_set_unique}"
            )
        }
    };
    let insert_sql =
        format!("INSERT INTO {table} ({columns_csv}) VALUES ({placeholders}) {conflict_clause}");
    // Last-write-wins guard. Without `unique`, look up the local row by pk.
    // With `unique`, the incoming row may match a local row by EITHER pk or the
    // natural key (they can be different rows), so consider both and keep the
    // newest local `lww` — never regress fresher local data.
    let (lww_select_sql, lww_lookup_params) = match &cfg.unique {
        None => (
            format!("SELECT {} FROM {table} WHERE {pk} = ?1", cfg.lww),
            {
                let pk_ident = Ident::new(&cfg.pk, Span::call_site());
                quote! { #crate_path::rusqlite::params![row.#pk_ident] }
            },
        ),
        Some(unique) => (
            format!(
                "SELECT {} FROM {table} WHERE {pk} = ?1 OR {unique} = ?2 \
                 ORDER BY {} DESC LIMIT 1",
                cfg.lww, cfg.lww
            ),
            {
                let pk_ident = Ident::new(&cfg.pk, Span::call_site());
                let unique_ident = Ident::new(unique, Span::call_site());
                quote! { #crate_path::rusqlite::params![row.#pk_ident, row.#unique_ident] }
            },
        ),
    };

    let expanded = quote! {
        const _: () = {
            // The replication bundle is a heterogeneous registry of entity rows,
            // so the wire payload is genuinely free-form JSON at this boundary.
            #[allow(clippy::disallowed_types)]
            impl #ty {
                fn __replicate_export(
                    conn: &#crate_path::rusqlite::Connection,
                ) -> #crate_path::anyhow::Result<#crate_path::serde_json::Value> {
                    let mut stmt = conn.prepare(#select_sql)?;
                    let rows = stmt.query_map([], |row| {
                        ::std::result::Result::Ok(#ty {
                            #( #field_idents: row.get(#get_indices)?, )*
                        })
                    })?;
                    let all: ::std::vec::Vec<#ty> =
                        rows.collect::<#crate_path::rusqlite::Result<::std::vec::Vec<_>>>()?;
                    ::std::result::Result::Ok(#crate_path::serde_json::to_value(all)?)
                }

                fn __replicate_merge(
                    conn: &#crate_path::rusqlite::Connection,
                    rows: #crate_path::serde_json::Value,
                ) -> #crate_path::anyhow::Result<usize> {
                    use #crate_path::rusqlite::OptionalExtension;
                    let rows: ::std::vec::Vec<#ty> = #crate_path::serde_json::from_value(rows)?;
                    let mut merged = 0usize;
                    for row in &rows {
                        let existing: ::std::option::Option<::std::string::String> = conn
                            .query_row(#lww_select_sql, #lww_lookup_params, |r| r.get(0))
                            .optional()?;
                        // Last-write-wins: skip when our copy is at least as new.
                        if let ::std::option::Option::Some(local) = &existing
                            && row.#lww_ident <= *local
                        {
                            continue;
                        }
                        conn.execute(
                            #insert_sql,
                            #crate_path::rusqlite::params![ #( row.#field_idents, )* ],
                        )?;
                        merged += 1;
                    }
                    ::std::result::Result::Ok(merged)
                }
            }

            #crate_path::inventory::submit! {
                #crate_path::ReplicatedRegistration {
                    name: #table,
                    export: #ty::__replicate_export,
                    merge: #ty::__replicate_merge,
                }
            }
        };
    };
    Ok(expanded)
}

fn expand(attr: ToolAttr, item: ItemFn) -> syn::Result<TokenStream2> {
    let crate_path = &attr.crate_path;
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

    // Explicit `title = "..."` from the macro attr. Emitted as
    // `Option<&'static str>` so the OpenAPI renderer can fall back to the
    // tool name when no title is set. Kept distinct from `description` so
    // authors can have a concise nav label AND a full markdown body.
    let title_tokens = match attr.title.as_ref() {
        Some(s) => quote! { ::core::option::Option::Some(#s) },
        None => quote! { ::core::option::Option::None },
    };

    let domain = attr.domain;
    let verb = attr.verb;
    let tool_name = format!("{}.{}", domain.value(), verb.value());
    let remote_ok_lit = attr.remote_ok;
    let data_mutation_lit = attr.data_mutation;
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
    let ctx_param = quote! { #ctx_param_name: &#crate_path::contract::ToolCtx };

    // Peer dispatch is universal: every tool with `remote_ok = true` (i.e.
    // not `local_only`) gets the proxy stanza. The trigger lives on
    // `ToolCtx::peer()` — populated by the CLI `--peer <h>` flag, REST
    // `X-Orca-Peer` header, or MCP envelope — so individual Args structs no
    // longer carry a `peer_id` field. `local_only = true` opts out.
    let emit_peer_dispatch = attr.remote_ok;
    if attr.refresh_runtime && !emit_peer_dispatch {
        return Err(syn::Error::new_spanned(
            &item.sig.inputs,
            "refresh_runtime=true is meaningless on a local_only tool",
        ));
    }
    let refresh_runtime_stanza = if attr.refresh_runtime {
        quote! {
            // Best-effort: force-refresh the peer's runtime snapshot so the
            // UI reflects state mutated by this tool immediately instead of
            // waiting for the next mesh poll. Default trait impl is a no-op;
            // pod's PodRemoteExec fetches `system.detail` and updates its
            // in-memory runtime cache. The backoff loop covers the
            // daemon-restart gap for tools that swap the peer's binary.
            let __svc_refresh = ::std::sync::Arc::clone(&__svc);
            let __peer_refresh = __peer_id.clone();
            #crate_path::reactor::spawn_detached(async move {
                for __delay_ms in [500u64, 2000, 5000, 10_000, 20_000] {
                    #crate_path::time::sleep(
                        ::std::time::Duration::from_millis(__delay_ms)
                    ).await;
                    if __svc_refresh
                        .refresh_peer_runtime(&__peer_refresh)
                        .await
                        .is_ok()
                    {
                        return;
                    }
                }
            });
        }
    } else {
        quote! {}
    };
    // local_only tools must reject `--peer <h>` clearly rather than silently
    // running on the controller. The opt-out is intentional, the user's
    // intent isn't — surface it.
    let local_only_reject_stanza = if !emit_peer_dispatch {
        quote! {
            if let ::core::option::Option::Some(__peer) = #ctx_param_name.peer() {
                return ::core::result::Result::Err(#crate_path::anyhow::anyhow!(
                    "tool `{}` is local_only and cannot be dispatched to peer `{}`",
                    #tool_name, __peer,
                ));
            }
        }
    } else {
        quote! {}
    };
    let peer_dispatch_stanza = if emit_peer_dispatch {
        quote! {
            if let ::core::option::Option::Some(__peer_id) =
                #ctx_param_name.peer().map(::std::string::ToString::to_string)
            {
                let __svc = #ctx_param_name
                    .service::<::std::sync::Arc<dyn #crate_path::contract::RemoteExec>>()?;
                let __args_value = #crate_path::serde_json::to_value(&#args_forward)
                    .map_err(|e| #crate_path::anyhow::anyhow!("peer_dispatch: serialize args: {e}"))?;
                // Forward the ctx's ambient operator identity; the transport
                // mints a signed caller token from it (project-remote-exec-full-fix
                // S1–S4). `None` on unauthenticated paths.
                let __out_value = __svc
                    .exec(
                        &__peer_id,
                        #tool_name,
                        __args_value,
                        #ctx_param_name.caller(),
                        #ctx_param_name.correlation_id().map(::std::string::ToString::to_string),
                    )
                    .await?;
                let __out: #output_ty = #crate_path::serde_json::from_value(__out_value)
                    .map_err(|e| #crate_path::anyhow::anyhow!(
                        "peer_dispatch: decode {} output from peer {}: {}",
                        #tool_name, __peer_id, e,
                    ))?;
                #refresh_runtime_stanza
                return ::core::result::Result::Ok(__out);
            }
        }
    } else {
        quote! {}
    };

    // Doc string keeps the original — we just relocate the description into
    // the const.
    let inner_fn = item;

    // CLI behaviour: default emits register_op! unconditionally; `manual`/
    // `skip` mirror the existing semantics. The `#[cfg(feature = "cli")]`
    // gate was removed because CLI/MCP/REST are structurally automatic
    // from `#[orca_tool]` — see feedback_rest_verbs_for_tool_surfaces and
    // the user's directive that specifying `cli` at all is the bug. The
    // emission depends on `clap`, which `dispatch` already takes as a
    // hard dep — no downstream feature toggling is needed.
    let cli_block = match attr.cli_mode.as_ref().map(|i| i.to_string()).as_deref() {
        Some("manual") | Some("skip") => quote! {},
        _ => quote! {
            const _: () = {
                #crate_path::dispatch::register_op! {
                    crate_path: #crate_path,
                    tool: #zst_ident,
                    domain: #domain,
                    verb: #verb,
                    summary: <#zst_ident as #crate_path::contract::OrcaToolDef>::DESCRIPTION,
                }
            };
        },
    };

    // OpenAPI emission — unconditional (the spec is built from schemars, no
    // native deps needed). Every tool gets one `/api/v1/<NAME>` POST entry
    // injected into the spec at runtime.
    let openapi_block = quote! {
        #crate_path::inventory::submit! {
            #crate_path::dispatch::openapi::OpenApiToolRegistration {
                name: #tool_name,
                title: #title_tokens,
                description: #description,
                domain: #domain,
                args_schema: || {
                    #crate_path::serde_json::to_value(
                        #crate_path::schemars::schema_for!(<#zst_ident as #crate_path::contract::OrcaToolDef>::Args)
                    ).unwrap_or(#crate_path::serde_json::Value::Object(#crate_path::serde_json::Map::new()))
                },
                output_schema: || {
                    #crate_path::serde_json::to_value(
                        #crate_path::schemars::schema_for!(<#zst_ident as #crate_path::contract::OrcaToolDef>::Output)
                    ).unwrap_or(#crate_path::serde_json::Value::Object(#crate_path::serde_json::Map::new()))
                },
            }
        }
    };

    let expanded = quote! {
        #inner_fn

        #[allow(non_camel_case_types)]
        pub struct #zst_ident;

        impl #crate_path::contract::OrcaToolDef for #zst_ident {
            const NAME: &'static str = #tool_name;
            const DESCRIPTION: &'static str = #description;
            const REMOTE_OK: bool = #remote_ok_lit;
            // local_only is the inverse of remote_ok by convention (a tool
            // that can't be dispatched to a remote peer also can't be
            // proxied through the local daemon's HTTP — it must run in the
            // calling process). Same lit, opposite polarity.
            const LOCAL_ONLY: bool = !#remote_ok_lit;
            const DATA_MUTATION: bool = #data_mutation_lit;
            #role_const
            type Args = #args_ty;
            type Output = #output_ty;
        }

        impl #crate_path::contract::OrcaOp for #zst_ident {
            const DOMAIN: &'static str = #domain;
            const VERB: &'static str = #verb;
        }

        #[#crate_path::async_trait::async_trait]
        impl #crate_path::contract::OrcaTool for #zst_ident {
            async fn run(
                #args_param,
                #ctx_param,
            ) -> #crate_path::anyhow::Result<#output_ty> {
                #local_only_reject_stanza
                #peer_dispatch_stanza
                #fn_ident(#args_forward, #ctx_param_name).await
            }
        }

        #crate_path::inventory::submit! {
            #crate_path::dispatch::ToolRegistration {
                name: #tool_name,
                make_erased: || ::std::boxed::Box::new(
                    #crate_path::dispatch::ToolWrapper::<#zst_ident>(::std::marker::PhantomData)
                ),
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

    // ── orca_async ────────────────────────────────────────────────────────────

    #[test]
    fn orca_async_rewrites_trait_required_and_provided_methods() {
        let out = orca_async::expand(quote! {
            pub trait Backend: Send + Sync {
                fn name(&self) -> &str;
                async fn emit(&self, event: &Event) -> Result<Msg, Err>;
                async fn ping(&self) -> bool { true }
            }
        })
        .to_string();
        // The sync method is untouched.
        assert!(out.contains("fn name"));
        // async fns lose `async` and gain a boxed Send future return.
        assert!(!out.contains("async fn"));
        assert!(out.contains("Pin") && out.contains("dyn") && out.contains("Future"));
        assert!(out.contains("Send"));
        // Borrowed params/receiver get lifetimes tied to 'async_trait.
        assert!(out.contains("'async_trait"));
        assert!(out.contains("Self : 'async_trait") || out.contains("Self: 'async_trait"));
        // The provided method's body is wrapped in a pinned async block.
        assert!(out.contains("Box :: pin") || out.contains("Box::pin"));
    }

    #[test]
    fn orca_async_wraps_impl_bodies_and_leaves_sync_alone() {
        let out = orca_async::expand(quote! {
            impl Backend for NtfyBackend {
                fn name(&self) -> &str { &self.name }
                async fn emit(&self, event: &Event) -> Result<Msg, Err> { send(event).await }
            }
        })
        .to_string();
        assert!(!out.contains("async fn"));
        assert!(out.contains("Box :: pin") || out.contains("Box::pin"));
        // The non-async accessor body is unchanged (not wrapped).
        assert!(out.contains("& self . name"));
    }

    #[test]
    fn orca_async_rejects_non_trait_non_impl() {
        let out = orca_async::expand(quote! {
            struct S;
        })
        .to_string();
        assert!(out.contains("compile_error"));
    }

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
        // remote_ok defaults to true (2026-05-28).
        assert!(attr.remote_ok);
        assert!(attr.cli_mode.is_none());
        assert!(attr.role.is_none());
    }

    #[test]
    fn tool_attr_local_only_flips_remote_ok_off() {
        let attr = parse_attr(quote!(domain = "x", verb = "y", local_only = true)).unwrap();
        assert!(!attr.remote_ok);
    }

    #[test]
    fn tool_attr_local_only_false_is_noop() {
        let attr = parse_attr(quote!(domain = "x", verb = "y", local_only = false)).unwrap();
        assert!(attr.remote_ok);
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
    fn tool_attr_defaults_crate_path_to_plugin_toolkit() {
        let attr = parse_attr(quote!(domain = "x", verb = "y")).unwrap();
        let crate_path = &attr.crate_path;
        let rendered = quote!(#crate_path).to_string();
        assert!(
            rendered.contains("plugin_toolkit"),
            "expected default crate_path to be ::plugin_toolkit, got: {rendered}"
        );
    }

    #[test]
    fn tool_attr_parses_crate_path_override() {
        let attr = parse_attr(quote!(domain = "x", verb = "y", crate = ::macro_runtime)).unwrap();
        let crate_path = &attr.crate_path;
        let rendered = quote!(#crate_path).to_string();
        assert!(
            rendered.contains("macro_runtime"),
            "expected crate_path to be ::macro_runtime, got: {rendered}"
        );
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

    // ── endpoint_tool ───────────────────────────────────────────────────────────

    #[test]
    fn endpoint_tool_generates_args_struct_and_wrapper() {
        let out = endpoint_tool::expand(
            quote! { domain = "home-assistant", verb = "entities" },
            quote! {
                /// List entities.
                async fn ha_entities(client: Client, #[arg(long)] domain: Option<String>) -> Result<JsonAny> {
                    Ok(client.entity_list(domain.as_deref()).await?.into())
                }
            },
        )
        .to_string();
        // Args struct named from the fn, with the always-present endpoint field.
        assert!(out.contains("struct HaEntitiesArgs"), "got: {out}");
        assert!(out.contains("pub endpoint : String") || out.contains("pub endpoint: String"));
        // The extra param becomes a field carrying its forwarded #[arg] attr.
        assert!(out.contains("pub domain : Option") || out.contains("pub domain: Option"));
        // plugin_struct(args) + orca_tool attrs are emitted (composed, not reimplemented).
        assert!(out.contains("plugin_struct (args)") || out.contains("plugin_struct(args)"));
        assert!(out.contains("orca_tool"));
        // The wrapper resolves the client and binds the arg.
        assert!(
            out.contains("make_client (& args . endpoint)")
                || out.contains("make_client(&args.endpoint)")
        );
        assert!(
            out.contains("let domain = args . domain") || out.contains("let domain = args.domain")
        );
        // Doc comment is preserved on the tool fn.
        assert!(out.contains("List entities"));
    }

    #[test]
    fn endpoint_tool_honors_resolve_override() {
        let out = endpoint_tool::expand(
            quote! { domain = "d", verb = "v", resolve = my_client },
            quote! {
                async fn t(client: C) -> Result<()> { Ok(()) }
            },
        )
        .to_string();
        assert!(
            out.contains("my_client (& args . endpoint)")
                || out.contains("my_client(&args.endpoint)")
        );
        // `resolve` is consumed, not forwarded to orca_tool.
        assert!(!out.contains("resolve ="), "got: {out}");
    }

    #[test]
    fn endpoint_tool_rejects_missing_client_param() {
        let out = endpoint_tool::expand(
            quote! { domain = "d", verb = "v" },
            quote! { async fn t() -> Result<()> { Ok(()) } },
        )
        .to_string();
        assert!(out.contains("compile_error"), "got: {out}");
    }
}
