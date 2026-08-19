//! Pure backend-descriptor builders that build a
//! [`BackendDef`](crate::abi::BackendDef) from a plugin's contract declarations,
//! with no reactor or transport dependency.
//!
//! A subprocess plugin advertises its `unit` / `topology` / `host_facts` /
//! `service_identity` backends by walking its contract declarations and
//! assembling a `BackendDef` — these builders do exactly that, so they are gated
//! on `tools` alone and link neither tokio nor any transport.
#![allow(clippy::disallowed_types)]

use serde_json as sj;

/// `backends()` payload for a plugin contributing no domain backend (a pure
/// tool-surface plugin): the empty array the loader also synthesizes.
pub const EMPTY_BACKENDS: &str = "[]";

/// `schemas()` payload for a plugin declaring no plugin-scoped SQL tables: the
/// empty declaration the loader synthesizes for a plugin predating the field.
pub const EMPTY_SCHEMAS: &str = r#"{"namespace":"","tables":[]}"#;

/// Serialize a plugin's declared SQL tables into the `schema_json` a
/// `serve_tool_plugin! { …, schemas: … }` call hands to core, which materializes
/// them through `db::plugin_tables` at load (create-if-absent + additive
/// migration). `namespace` is the plugin's isolation key — every physical table
/// is derived as `plug__<namespace>__<table>`, so it can name neither a core
/// table nor another plugin's. `tables` are the declared shapes (real typed
/// columns + indexes, never a KV blob). Falls back to [`EMPTY_SCHEMAS`] only if
/// serialization somehow fails, so a plugin never ships an unparsable decl.
pub fn schemas_json(namespace: &str, tables: Vec<crate::abi::TableDef>) -> String {
    let decl = crate::abi::SchemaDecl {
        namespace: namespace.to_string(),
        tables,
    };
    sj::to_string(&decl).unwrap_or_else(|_| EMPTY_SCHEMAS.to_string())
}

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
    use crate::storage::{Capability, MountStyle, StorageKind};

    let kind = match backend.kind() {
        StorageKind::NetworkShare => "network_share",
        StorageKind::DiskStorage => "disk_storage",
        StorageKind::Object => "object",
    };
    let mount_style = match backend.mount_style() {
        MountStyle::KernelMount => "kernel_mount",
        MountStyle::UserspaceProcess => "userspace_process",
    };
    let capabilities = backend
        .capabilities()
        .into_iter()
        .map(|c| {
            match c {
                Capability::List => "list",
                Capability::Exports => "exports",
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
        mount_style: mount_style.to_string(),
        net_fstypes: backend.net_fstypes(),
        default_source_port: backend.default_source_port(),
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
    }
}

/// Build the `secrets_backend`-domain [`BackendDef`](crate::abi::BackendDef) a
/// plugin advertises so orca resolves secrets whose `backend` kind matches
/// `kind` (e.g. `onepassword`) by dispatching to the plugin.
///
/// The domain routes `{invoke_prefix}.resolve`
/// ([`RESOLVE_OP`](crate::contract::secrets_backend::RESOLVE_OP)) back to the
/// plugin, so a plugin lights secrets resolution up by (1) exposing a `resolve`
/// op that takes `{"ref_path": <string>}` and returns the raw secret value as a
/// JSON string and (2) advertising this def.
pub fn secrets_backend_def(kind: &str, invoke_prefix: &str) -> crate::abi::BackendDef {
    crate::abi::BackendDef {
        domain: "secrets_backend".to_string(),
        name: kind.to_string(),
        kind: String::new(),
        runtime: String::new(),
        endpoint: String::new(),
        capabilities: vec![crate::contract::secrets_backend::RESOLVE_OP.to_string()],
        invoke_prefix: invoke_prefix.to_string(),
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload advertising a secrets backend.
pub fn secrets_backends_json(kind: &str, invoke_prefix: &str) -> String {
    sj::to_string(&[secrets_backend_def(kind, invoke_prefix)]).unwrap_or_else(|_| "[]".to_string())
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
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload advertising a service-identity
/// backend.
pub fn service_identity_backends_json(name: &str, invoke_prefix: &str) -> String {
    sj::to_string(&[service_identity_backend_def(name, invoke_prefix)])
        .unwrap_or_else(|_| "[]".to_string())
}

/// Build the `backup_kind`-domain [`BackendDef`](crate::abi::BackendDef) a plugin
/// advertises to contribute a backup KIND (the WHAT axis — `vm` / `lxc` /
/// `flash`) that `orca backup --kind <kind>` fans out over.
///
/// `kind` is the KIND name; the domain routes these ops back to the plugin as
/// `{invoke_prefix}.{op}`:
/// * `instances` → `[]` (String names; omit to accept the `["default"]` default)
/// * `layout` `{instance}` → `[]` (the `<category>/<class>/<name>` path segments)
/// * `backup` `{payload_dir, instance}` → a `BackupOutcome` JSON — the plugin
///   writes the backup INTO the host-local `payload_dir` (shared filesystem)
/// * `restore` `{payload_dir, instance}` → any/`null`
///
/// INVARIANT: `name == kind` for backup backends. The host records this backend
/// by `name` and, on unload, deregisters by that name, while the provider
/// registry keys by `kind`; the two must match or unload orphans the provider.
/// This helper sets them equal — hand-rolled `BackendDef`s that diverge are
/// rejected at registration.
pub fn backup_kind_backend_def(kind: &str, invoke_prefix: &str) -> crate::abi::BackendDef {
    crate::abi::BackendDef {
        domain: "backup_kind".to_string(),
        name: kind.to_string(),
        kind: kind.to_string(),
        runtime: String::new(),
        endpoint: String::new(),
        capabilities: vec![
            "instances".to_string(),
            "layout".to_string(),
            "backup".to_string(),
            "restore".to_string(),
        ],
        invoke_prefix: invoke_prefix.to_string(),
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload advertising a backup KIND.
pub fn backup_kind_backends_json(kind: &str, invoke_prefix: &str) -> String {
    sj::to_string(&[backup_kind_backend_def(kind, invoke_prefix)])
        .unwrap_or_else(|_| "[]".to_string())
}

/// Build the `backup_target`-domain [`BackendDef`](crate::abi::BackendDef) a
/// plugin advertises to contribute a backup TARGET (the WHERE axis — `nfs` /
/// `smb` / `s3` / `pbs`) the generic store writes beneath.
///
/// `kind` is the target kind; the domain routes these ops back as
/// `{invoke_prefix}.{op}`:
/// * `open` `{name}` → `{root}` — the host-local root path the plugin
///   provisioned; the generic store owns layout/retention beneath it
/// * `sync` / `refresh` `{name}` → any/`null` (post-write / pre-read hooks)
/// * `fits` `{placement}` → `bool` (omit to fit everywhere, as `local` does)
/// * `default_retention` / `default_schedule` `{name}` → the target's default,
///   or `null` to fall through to the unit policy default
/// * `available` → `[]` (`TargetLocation`s to offer in the target picker)
/// * `backing_key` `{name}` → a globally stable backing identity for fleet-wide
///   collision detection
pub fn backup_target_backend_def(kind: &str, invoke_prefix: &str) -> crate::abi::BackendDef {
    crate::abi::BackendDef {
        domain: "backup_target".to_string(),
        name: kind.to_string(),
        kind: kind.to_string(),
        runtime: String::new(),
        endpoint: String::new(),
        capabilities: vec![
            "open".to_string(),
            "sync".to_string(),
            "refresh".to_string(),
            "fits".to_string(),
            "default_retention".to_string(),
            "default_schedule".to_string(),
            "available".to_string(),
            "backing_key".to_string(),
        ],
        invoke_prefix: invoke_prefix.to_string(),
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload advertising a backup TARGET.
pub fn backup_target_backends_json(kind: &str, invoke_prefix: &str) -> String {
    sj::to_string(&[backup_target_backend_def(kind, invoke_prefix)])
        .unwrap_or_else(|_| "[]".to_string())
}

/// Derive the `deploy_target`-domain [`BackendDef`](crate::abi::BackendDef) from
/// a live [`DeployTarget`](crate::deploy_target::DeployTarget).
///
/// The descriptor orca's loader registers is *exactly* what the target declares —
/// its three independent identity axes (`host` × `runtime` × `kind`) plus the
/// capabilities it advertises, none restated in a drift-prone literal. The
/// deploy domain maps onto `BackendDef`'s generic axes: `name` carries the `host`
/// (the loader keys its unload on the backend name, which for this domain is the
/// host), `runtime` and `kind` carry the discrete management axes as their own
/// strings, and `capabilities` carries the launch/stop/restart/… CSV. The
/// loader's `register_deploy_target_backend` reads these back and parses each
/// string into the domain enum, so a typo surfaces at load, not first use.
///
/// A plugin lights a deploy target up by (1) exposing the proxied ops
/// ([`OP_LAUNCH`](crate::deploy_target::OP_LAUNCH) / `OP_STOP` / `OP_RESTART`)
/// through [`deploy_target::dispatch_op`](crate::deploy_target::dispatch_op) and
/// (2) advertising this def.
pub fn deploy_backend_def(
    target: &dyn crate::deploy_target::DeployTarget,
    invoke_prefix: &str,
) -> crate::abi::BackendDef {
    let capabilities = target
        .capabilities()
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();

    crate::abi::BackendDef {
        domain: "deploy_target".to_string(),
        name: target.host().to_string(),
        kind: target.kind().as_str().to_string(),
        runtime: target.runtime().as_str().to_string(),
        endpoint: target.endpoint(),
        capabilities,
        provisioning: target.provisioning(),
        invoke_prefix: invoke_prefix.to_string(),
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload advertising a deploy target.
pub fn deploy_backends_json(
    target: &dyn crate::deploy_target::DeployTarget,
    invoke_prefix: &str,
) -> String {
    sj::to_string(&[deploy_backend_def(target, invoke_prefix)]).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_json_serializes_a_declaration_the_daemon_parses() {
        use crate::abi::{ColumnDef, TableDef};
        let table = TableDef {
            table: "deploy_target".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    sql_type: "TEXT".to_string(),
                    not_null: true,
                    primary_key: true,
                    default: None,
                },
                ColumnDef {
                    name: "cores".to_string(),
                    sql_type: "INTEGER".to_string(),
                    not_null: true,
                    primary_key: false,
                    default: Some("1".to_string()),
                },
            ],
            indexes: vec![],
        };
        let json = schemas_json("proxmox", vec![table]);
        // Round-trips into the same SchemaDecl shape `serve()` parses.
        let decl: crate::abi::SchemaDecl =
            serde_json::from_str(&json).expect("schemas_json is a valid SchemaDecl");
        assert_eq!(decl.namespace, "proxmox");
        assert_eq!(decl.tables.len(), 1);
        assert_eq!(decl.tables[0].table, "deploy_target");
        assert_eq!(decl.tables[0].columns.len(), 2);
        assert!(decl.tables[0].columns[0].primary_key);
    }

    #[test]
    fn empty_schemas_is_a_valid_empty_declaration() {
        let decl: crate::abi::SchemaDecl =
            serde_json::from_str(EMPTY_SCHEMAS).expect("EMPTY_SCHEMAS parses");
        assert!(decl.namespace.is_empty());
        assert!(decl.tables.is_empty());
    }

    use crate::contract::BoxFuture;
    use crate::contract::unit::{
        KindDeclaration, UnitDescriptor, UnitProvider, VerbArgs, VerbDecl, VerbOutcome,
    };

    struct DemoProvider;

    impl UnitProvider for DemoProvider {
        fn name(&self) -> &str {
            "demo"
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            vec![
                KindDeclaration {
                    kind: "stack".into(),
                    backup_spec: None,
                    verbs: vec![VerbDecl::list(), VerbDecl::detail()],
                },
                // Second kind repeats `list` — the capability CSV must dedup it.
                KindDeclaration {
                    kind: "container".into(),
                    backup_spec: None,
                    verbs: vec![VerbDecl::list()],
                },
            ]
        }
        fn units(&self) -> BoxFuture<'_, crate::anyhow::Result<Vec<UnitDescriptor>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn invoke(&self, _args: VerbArgs) -> BoxFuture<'_, crate::anyhow::Result<VerbOutcome>> {
            Box::pin(async { unreachable!("not exercised by this test") })
        }
    }

    #[test]
    fn unit_backend_def_is_derived_from_the_provider() {
        let def = unit_backend_def(&DemoProvider, "demo.__unit");
        assert_eq!(def.domain, "unit");
        assert_eq!(def.name, "demo");
        assert_eq!(def.invoke_prefix, "demo.__unit");
        // Declared kinds ride the runtime axis, in declaration order.
        assert_eq!(def.runtime, "stack,container");
        // Verbs are the deduped, sorted union across kinds.
        assert_eq!(def.capabilities, vec!["detail", "list"]);
    }

    #[test]
    fn unit_backends_json_wraps_the_def_in_a_one_element_array() {
        let json = unit_backends_json(&DemoProvider, "demo.__unit");
        assert!(json.starts_with('['));
        assert!(json.contains("\"domain\":\"unit\""));
        assert!(json.contains("\"name\":\"demo\""));
    }

    #[test]
    fn topology_backend_def_advertises_the_collect_op() {
        let def = topology_backend_def("demo", "demo");
        assert_eq!(def.domain, "topology");
        assert_eq!(def.name, "demo");
        assert_eq!(def.invoke_prefix, "demo");
        assert_eq!(
            def.capabilities,
            vec![crate::contract::topology::COLLECT_OP.to_string()]
        );
    }

    #[test]
    fn secrets_backend_def_advertises_the_resolve_op() {
        let def = secrets_backend_def("onepassword", "op");
        assert_eq!(def.domain, "secrets_backend");
        assert_eq!(def.name, "onepassword");
        assert_eq!(def.invoke_prefix, "op");
        assert_eq!(
            def.capabilities,
            vec![crate::contract::secrets_backend::RESOLVE_OP.to_string()]
        );
    }

    // ── deploy_target ────────────────────────────────────────────────────────

    use crate::deploy_target::{
        DeployCapability, DeployError, DeployOutcome, DeployTarget, Runtime, TargetKind,
        WorkloadSpec,
    };

    // A plugin-side deploy target. `async_trait` is banned in orca's own code,
    // but `DeployTarget` is an existing `#[async_trait]` trait in the deploy
    // crate; a plugin implements it, so the test does too (hand-rolling the
    // boxed-future impl would not exercise the real seam).
    struct MockDeploy;

    #[crate::async_trait::async_trait]
    impl DeployTarget for MockDeploy {
        fn host(&self) -> &str {
            "host-d"
        }
        fn runtime(&self) -> Runtime {
            Runtime::Docker
        }
        fn kind(&self) -> TargetKind {
            TargetKind::Dockge
        }
        fn capabilities(&self) -> Vec<DeployCapability> {
            vec![
                DeployCapability::Launch,
                DeployCapability::Stop,
                DeployCapability::Restart,
            ]
        }
        fn endpoint(&self) -> String {
            "dockge://host-d:5001".into()
        }
        async fn launch(&self, spec: &WorkloadSpec) -> Result<DeployOutcome, DeployError> {
            Ok(DeployOutcome {
                workload: spec.name.clone(),
                id: Some("42".into()),
                state: Some("running".into()),
                detail: None,
            })
        }
        async fn stop(&self, workload: &str) -> Result<DeployOutcome, DeployError> {
            Ok(DeployOutcome {
                workload: workload.to_string(),
                id: None,
                state: Some("stopped".into()),
                detail: None,
            })
        }
        async fn restart(&self, workload: &str) -> Result<DeployOutcome, DeployError> {
            Ok(DeployOutcome {
                workload: workload.to_string(),
                id: None,
                state: Some("running".into()),
                detail: None,
            })
        }
    }

    #[test]
    fn deploy_backend_def_is_derived_from_the_target() {
        let def = deploy_backend_def(&MockDeploy, "dockge.__deploy.host-d");
        assert_eq!(def.domain, "deploy_target");
        // host rides `name` (the loader keys unload on it).
        assert_eq!(def.name, "host-d");
        assert_eq!(def.runtime, "docker");
        assert_eq!(def.kind, "dockge");
        assert_eq!(def.endpoint, "dockge://host-d:5001");
        assert_eq!(def.invoke_prefix, "dockge.__deploy.host-d");
        assert_eq!(def.capabilities, vec!["launch", "stop", "restart"]);

        let json = deploy_backends_json(&MockDeploy, "dockge.__deploy.host-d");
        assert!(json.contains("\"domain\":\"deploy_target\""));
        assert!(json.contains("\"runtime\":\"docker\""));
    }

    // Full round-trip: build the def from the target, hand it to the loader's
    // `register_from_def` with a thunk that routes bare ops back through the
    // plugin-side `dispatch_op`, then drive launch/stop/restart on the registered
    // proxy — exactly the path the loader wires for a loaded subprocess plugin.
    // The thunk drives `dispatch_op` on a tokio-free poll loop (the mock's
    // futures are ready immediately) so no runtime is nested inside the proxy's
    // `spawn_blocking`.
    #[cfg(feature = "in-process")]
    #[test]
    fn deploy_backend_def_registers_and_answers_through_dispatch() {
        use crate::deploy_target::{
            self, InvokeThunk, TargetId, dispatch_op, register_from_def, target as lookup_target,
        };
        use std::sync::Arc;

        let def = deploy_backend_def(&MockDeploy, "dockge.__deploy.host-d");

        let thunk: InvokeThunk = Arc::new(|op: &str, args_json: String| {
            futures_block(dispatch_op(&MockDeploy, op, &args_json)).map_err(DeployError::Transport)
        });

        register_from_def(
            def.name.clone(),
            &def.runtime,
            &def.kind,
            def.endpoint.clone(),
            &def.capabilities,
            def.provisioning.clone(),
            thunk,
        )
        .expect("register deploy target from def");

        let id = TargetId {
            host: "host-d".into(),
            runtime: Runtime::Docker,
            kind: TargetKind::Dockge,
        };
        let proxy = lookup_target(&id).expect("registered target is retrievable");
        assert!(proxy.supports(DeployCapability::Launch));

        let spec = WorkloadSpec {
            name: "web".into(),
            ..Default::default()
        };
        let launched =
            crate::reactor::block_on(async { proxy.launch(&spec).await }).expect("launch");
        assert_eq!(launched.workload, "web");
        assert_eq!(launched.id.as_deref(), Some("42"));
        assert_eq!(launched.state.as_deref(), Some("running"));

        let stopped = crate::reactor::block_on(async { proxy.stop("web").await }).expect("stop");
        assert_eq!(stopped.state.as_deref(), Some("stopped"));
        let restarted =
            crate::reactor::block_on(async { proxy.restart("web").await }).expect("restart");
        assert_eq!(restarted.state.as_deref(), Some("running"));

        assert!(deploy_target::deregister_target(&id));
    }

    // A proxmox LXC target that carries a typed provisioning profile and reads
    // it back at launch — proving the target's own config reaches the launch
    // path. Its `launch` embeds the node + cores it provisions against into the
    // outcome detail so a test can observe the config was consulted.
    struct MockProxmox {
        provisioning: crate::deploy_target::ProvisioningConfig,
    }

    #[crate::async_trait::async_trait]
    impl DeployTarget for MockProxmox {
        fn host(&self) -> &str {
            "host-e"
        }
        fn runtime(&self) -> Runtime {
            Runtime::Lxc
        }
        fn kind(&self) -> TargetKind {
            TargetKind::Proxmox
        }
        fn capabilities(&self) -> Vec<DeployCapability> {
            vec![DeployCapability::Launch]
        }
        fn endpoint(&self) -> String {
            "proxmox:pve/lxc".into()
        }
        fn provisioning(&self) -> Option<crate::deploy_target::ProvisioningConfig> {
            Some(self.provisioning.clone())
        }
        async fn launch(&self, spec: &WorkloadSpec) -> Result<DeployOutcome, DeployError> {
            let crate::deploy_target::ProvisioningConfig::Proxmox(p) = self
                .provisioning()
                .ok_or_else(|| DeployError::Other("proxmox target missing provisioning".into()))?;
            Ok(DeployOutcome {
                workload: spec.name.clone(),
                id: Some("101".into()),
                state: Some("running".into()),
                detail: Some(format!("node={} cores={}", p.node, p.cores)),
            })
        }
    }

    fn sample_proxmox_provisioning() -> crate::deploy_target::ProvisioningConfig {
        crate::deploy_target::ProvisioningConfig::Proxmox(
            crate::deploy_target::ProxmoxProvisioning {
                node: "pve".into(),
                endpoint: "https://pve:8006".into(),
                storage: "local-lvm".into(),
                cores: 4,
                memory_mb: 8192,
            },
        )
    }

    #[test]
    fn deploy_backend_def_carries_provisioning_from_the_target() {
        let target = MockProxmox {
            provisioning: sample_proxmox_provisioning(),
        };
        let def = deploy_backend_def(&target, "proxmox.__deploy.host-e");
        assert_eq!(def.runtime, "lxc");
        assert_eq!(def.kind, "proxmox");
        assert_eq!(def.provisioning, Some(sample_proxmox_provisioning()));

        // The typed profile survives the JSON round-trip the loader carries it
        // over — no opaque blob, no dropped fields.
        let json = deploy_backends_json(&target, "proxmox.__deploy.host-e");
        assert!(json.contains("\"proxmox\""));
        assert!(json.contains("\"node\":\"pve\""));
        assert!(json.contains("\"cores\":4"));
    }

    // Full seam: build the def (provisioning rides `BackendDef`), register it
    // through the loader path, then confirm the registered proxy advertises the
    // profile AND that launching drives the target's own config into the outcome
    // — the target-carries-params contract end to end.
    #[cfg(feature = "in-process")]
    #[test]
    fn provisioning_survives_registration_and_reaches_launch() {
        use crate::deploy_target::{
            self, InvokeThunk, ProvisioningConfig, TargetId, dispatch_op, register_from_def,
            target as lookup_target,
        };
        use std::sync::Arc;

        let def = deploy_backend_def(
            &MockProxmox {
                provisioning: sample_proxmox_provisioning(),
            },
            "proxmox.__deploy.host-e",
        );

        let thunk: InvokeThunk = Arc::new(|op: &str, args_json: String| {
            let target = MockProxmox {
                provisioning: sample_proxmox_provisioning(),
            };
            futures_block(dispatch_op(&target, op, &args_json)).map_err(DeployError::Transport)
        });

        register_from_def(
            def.name.clone(),
            &def.runtime,
            &def.kind,
            def.endpoint.clone(),
            &def.capabilities,
            def.provisioning.clone(),
            thunk,
        )
        .expect("register proxmox deploy target from def");

        let id = TargetId {
            host: "host-e".into(),
            runtime: Runtime::Lxc,
            kind: TargetKind::Proxmox,
        };
        let proxy = lookup_target(&id).expect("registered target is retrievable");

        // The profile is retrievable off the registered proxy…
        let ProvisioningConfig::Proxmox(p) =
            proxy.provisioning().expect("proxy advertises provisioning");
        assert_eq!(p.node, "pve");
        assert_eq!(p.storage, "local-lvm");
        assert_eq!(p.cores, 4);
        assert_eq!(p.memory_mb, 8192);

        // …and it reaches the launch path: the target consulted its own config.
        let spec = WorkloadSpec {
            name: "ct".into(),
            ..Default::default()
        };
        let launched =
            crate::reactor::block_on(async { proxy.launch(&spec).await }).expect("launch");
        assert_eq!(launched.detail.as_deref(), Some("node=pve cores=4"));

        assert!(deploy_target::deregister_target(&id));
    }

    // Tokio-free executor for the dispatch thunk: the mock's futures are ready on
    // first poll, so a trivial poll loop drives them without nesting a tokio
    // runtime inside the proxy's `spawn_blocking` thread.
    #[cfg(feature = "in-process")]
    fn futures_block<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VT)
        }
        static VT: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }
}
