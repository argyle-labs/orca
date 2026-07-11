//! Thin-profile backend-descriptor builders: the pure parts of the cdylib
//! export glue that build a [`BackendDef`](crate::abi::BackendDef) from a
//! plugin's contract declarations, with no reactor or FFI dependency.
//!
//! A **subprocess** plugin advertises its `unit` / `topology` / `host_facts` /
//! `service_identity` backends exactly as the in-process `export` glue does —
//! but it links no tokio and no `abi_stable`. These builders only walk the
//! contract declarations and assemble a `BackendDef`, so they belong here
//! (gated on `tools` alone) rather than inside [`export`](crate::export), whose
//! `runtime()` drags in the tokio reactor and forces the whole module behind
//! `in-process`.
//!
//! The in-process `export` module re-exports these so the cdylib export macros
//! keep resolving `$crate::export::{unit_backend_def, ...}` unchanged.
#![allow(clippy::disallowed_types)]

use serde_json as sj;

/// `backends()` payload for a plugin contributing no domain backend (a pure
/// tool-surface plugin): the empty array the loader also synthesizes.
pub const EMPTY_BACKENDS: &str = "[]";

/// `schemas()` payload for a plugin declaring no plugin-scoped SQL tables: the
/// empty declaration the loader synthesizes for a plugin predating the field.
pub const EMPTY_SCHEMAS: &str = r#"{"namespace":"","tables":[]}"#;

/// Derive a [`BackendDef`](crate::abi::BackendDef) from a live storage backend.
///
/// The descriptor orca's loader registers is *exactly* the backend's own
/// [`provider`](crate::storage::StorageBackend::provider) — kind, endpoint and
/// capabilities all come from the trait, so a backend plugin never restates
/// them in a hand-written literal that can drift. `..Default::default()` keeps
/// the literal forward-compatible with new `BackendDef` axes (e.g. the
/// deploy-target `runtime` field).
pub fn storage_backend_def(
    backend: &dyn crate::storage::StorageBackend,
    invoke_prefix: &str,
) -> crate::abi::BackendDef {
    use crate::storage::{Capability, StorageKind};

    let kind = match backend.kind() {
        StorageKind::NetworkShare => "network_share",
        StorageKind::DiskStorage => "disk_storage",
        StorageKind::Object => "object",
    };
    let capabilities = backend
        .capabilities()
        .into_iter()
        .map(|c| {
            match c {
                Capability::List => "list",
                Capability::Mount => "mount",
                Capability::Unmount => "unmount",
                Capability::Usage => "usage",
                Capability::Create => "create",
                Capability::Remove => "remove",
                Capability::RecoverStale => "recover_stale",
            }
            .to_string()
        })
        .collect();

    crate::abi::BackendDef {
        domain: "storage".to_string(),
        name: backend.name().to_string(),
        kind: kind.to_string(),
        endpoint: backend.endpoint(),
        capabilities,
        invoke_prefix: invoke_prefix.to_string(),
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload from a live storage backend.
pub fn storage_backends_json(
    backend: &dyn crate::storage::StorageBackend,
    invoke_prefix: &str,
) -> String {
    let def = storage_backend_def(backend, invoke_prefix);
    sj::to_string(&[def]).unwrap_or_else(|_| "[]".to_string())
}

/// Derive a [`BackendDef`](crate::abi::BackendDef) from a live service backend.
///
/// The descriptor orca registers is exactly the backend's own
/// [`descriptor`](crate::service::ServiceBackend::descriptor) — modalities,
/// port, endpoint and capabilities all come from the trait, never restated in a
/// drift-prone literal. The service domain reuses `BackendDef`'s generic axes:
/// `kind` carries the default port, `runtime` the supported-modality CSV.
pub fn service_backend_def(
    backend: &dyn crate::service::ServiceBackend,
    invoke_prefix: &str,
) -> crate::abi::BackendDef {
    let runtimes = backend
        .runtimes()
        .into_iter()
        .map(crate::service::runtime_str)
        .collect::<Vec<_>>()
        .join(",");
    let capabilities = backend
        .capabilities()
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();

    crate::abi::BackendDef {
        domain: "service".to_string(),
        name: backend.provider().to_string(),
        kind: backend.default_port().to_string(),
        runtime: runtimes,
        endpoint: backend.endpoint(),
        capabilities,
        invoke_prefix: invoke_prefix.to_string(),
    }
}

/// Serialize a one-backend `backends()` payload from a live service backend.
pub fn service_backends_json(
    backend: &dyn crate::service::ServiceBackend,
    invoke_prefix: &str,
) -> String {
    let def = service_backend_def(backend, invoke_prefix);
    sj::to_string(&[def]).unwrap_or_else(|_| "[]".to_string())
}

/// Six-verb name a declared [`Verb`](crate::contract::unit::Verb) advertises as
/// a `unit`-domain capability. Kept here (not on `Verb`) so the wire-facing
/// capability CSV lives at the export seam, next to the other `*_backend_def`
/// helpers, rather than leaking a display concern into the contract enum.
fn verb_capability(verb: crate::contract::unit::Verb) -> &'static str {
    use crate::contract::unit::Verb;
    match verb {
        Verb::List => "list",
        Verb::Detail => "detail",
        Verb::Create => "create",
        Verb::Update => "update",
        Verb::Delete => "delete",
        Verb::Upsert => "upsert",
    }
}

/// Derive a [`BackendDef`](crate::abi::BackendDef) from a live
/// [`UnitProvider`](crate::contract::unit::UnitProvider).
///
/// The descriptor orca's loader registers is *exactly* what the provider
/// declares: `name` is the provider name, the declared kinds ride the generic
/// `runtime` axis as a CSV, and the union of declared verbs (deduped, sorted)
/// rides `capabilities`. Nothing is restated in a drift-prone literal in the
/// plugin's `registration.rs` — add a kind or a verb to the provider and the
/// registered backend follows automatically.
pub fn unit_backend_def(
    provider: &dyn crate::contract::unit::UnitProvider,
    invoke_prefix: &str,
) -> crate::abi::BackendDef {
    let decls = provider.declarations();
    let runtime = decls
        .iter()
        .map(|d| d.kind.clone())
        .collect::<Vec<_>>()
        .join(",");
    let mut capabilities = decls
        .iter()
        .flat_map(|d| d.verbs.iter().map(|v| verb_capability(v.verb).to_string()))
        .collect::<Vec<_>>();
    capabilities.sort();
    capabilities.dedup();

    crate::abi::BackendDef {
        domain: "unit".to_string(),
        name: provider.name().to_string(),
        kind: String::new(),
        runtime,
        endpoint: String::new(),
        capabilities,
        invoke_prefix: invoke_prefix.to_string(),
    }
}

/// Serialize a one-backend `backends()` payload from a live unit provider.
pub fn unit_backends_json(
    provider: &dyn crate::contract::unit::UnitProvider,
    invoke_prefix: &str,
) -> String {
    let def = unit_backend_def(provider, invoke_prefix);
    sj::to_string(&[def]).unwrap_or_else(|_| "[]".to_string())
}

/// Build the `topology`-domain [`BackendDef`](crate::abi::BackendDef) a plugin
/// advertises so orca merges its `TopologyClaim`s into the fleet graph.
///
/// The topology domain routes `{invoke_prefix}.collect_claims`
/// ([`COLLECT_OP`](crate::contract::topology::COLLECT_OP)) back to the plugin,
/// so a plugin lights topology up by (1) exposing a `collect_claims` op that
/// returns `Vec<TopologyClaim>` JSON and (2) advertising this def. Standardized
/// here so dockge / unraid stop hand-writing the literal (and stop forgetting
/// to register it at all).
pub fn topology_backend_def(name: &str, invoke_prefix: &str) -> crate::abi::BackendDef {
    crate::abi::BackendDef {
        domain: "topology".to_string(),
        name: name.to_string(),
        kind: String::new(),
        runtime: String::new(),
        endpoint: String::new(),
        capabilities: vec![crate::contract::topology::COLLECT_OP.to_string()],
        invoke_prefix: invoke_prefix.to_string(),
    }
}

/// Serialize a one-backend `backends()` payload advertising a topology backend.
pub fn topology_backends_json(name: &str, invoke_prefix: &str) -> String {
    sj::to_string(&[topology_backend_def(name, invoke_prefix)]).unwrap_or_else(|_| "[]".to_string())
}

/// Build the `host_facts`-domain [`BackendDef`](crate::abi::BackendDef) a plugin
/// advertises so orca folds its [`HostFacts`](crate::contract::HostFacts) about
/// the local host into that host's mesh-propagated `system` snapshot.
///
/// The host-facts domain routes `{invoke_prefix}.get_facts`
/// ([`FACTS_OP`](crate::contract::host_facts::FACTS_OP)) back to the plugin, so
/// a plugin lights it up by (1) exposing a `get_facts` op returning a
/// `HostFacts` JSON and (2) advertising this def.
pub fn host_facts_backend_def(name: &str, invoke_prefix: &str) -> crate::abi::BackendDef {
    crate::abi::BackendDef {
        domain: "host_facts".to_string(),
        name: name.to_string(),
        kind: String::new(),
        runtime: String::new(),
        endpoint: String::new(),
        capabilities: vec![crate::contract::host_facts::FACTS_OP.to_string()],
        invoke_prefix: invoke_prefix.to_string(),
    }
}

/// Build the `service_identity`-domain [`BackendDef`](crate::abi::BackendDef) a
/// plugin advertises so orca correlates its runtime service registrations to the
/// containers/guests they run on.
///
/// The domain routes `{invoke_prefix}.list_registrations`
/// ([`LIST_OP`](crate::contract::service_identity::LIST_OP)) back to the plugin,
/// so a plugin lights service-identity up by (1) exposing a `list_registrations`
/// op that returns `Vec<ServiceRegistration>` JSON and (2) advertising this def.
pub fn service_identity_backend_def(name: &str, invoke_prefix: &str) -> crate::abi::BackendDef {
    crate::abi::BackendDef {
        domain: "service_identity".to_string(),
        name: name.to_string(),
        kind: String::new(),
        runtime: String::new(),
        endpoint: String::new(),
        capabilities: vec![crate::contract::service_identity::LIST_OP.to_string()],
        invoke_prefix: invoke_prefix.to_string(),
    }
}

/// Serialize a one-backend `backends()` payload advertising a service-identity
/// backend.
pub fn service_identity_backends_json(name: &str, invoke_prefix: &str) -> String {
    sj::to_string(&[service_identity_backend_def(name, invoke_prefix)])
        .unwrap_or_else(|_| "[]".to_string())
}
