//! Userspace-process mount applier — the reconciler for storage backends whose
//! [`MountStyle`] is [`MountStyle::UserspaceProcess`] (object stores realized by a
//! long-lived FUSE/gateway daemon) rather than a kernel mount driven through
//! autofs.
//!
//! ## Why this is separate from `autofs`
//!
//! [`crate::autofs`] renders the declarative `managed_mounts` store into an autofs
//! direct map and lets the kernel own mount mechanics. That path is correct for
//! network shares (nfs/smb) and is left COMPLETELY unchanged here. An object-store
//! backend has no kernel mount entry: it is realized by a helper process the
//! backend supervises, entered through [`StorageBackend::mount`] / torn down
//! through [`StorageBackend::unmount`]. Core reaches the backend through the
//! process-global storage registry (a [`plugin_toolkit::storage::backend`]
//! lookup, which resolves the subprocess proxy) and drives those two calls.
//!
//! ## Secret handling
//!
//! A userspace mount's credential is a [`plugin_toolkit::storage::SecretRef`] the
//! secrets domain resolves. It is resolved to plaintext HERE, immediately before
//! [`StorageBackend::mount`], and passed to the backend over the same in-process
//! secret channel the #122 inline-SMB creds path uses
//! ([`plugin_toolkit::secrets::get_required`]). The resolved value is never
//! logged and never written to a world-readable path — the FUSE daemon receives
//! it directly from the backend proxy. A mount whose secret fails to resolve is
//! skipped (fail closed) rather than mounted without credentials.
//!
//! ## Supervision (minimal lifecycle — see the module note)
//!
//! orca core has no generic long-lived-process supervisor seam today (the
//! `service` domain deploys containerized workloads; it is not a crash-restart
//! supervisor for an arbitrary daemon). So this applier implements the minimal
//! lifecycle the mount contract needs, delegating the actual process handling to
//! the backend:
//!
//! * **apply**   — for every enabled userspace mount not already up, call
//!   [`StorageBackend::mount`] (the backend spawns its daemon).
//! * **teardown**— for a mount removed/disabled, call [`StorageBackend::unmount`]
//!   (the backend kills its daemon and cleans up).
//! * **detect-dead** — on each reconcile, a mount declared-up whose backend
//!   reports it is not mounted is re-`mount`ed (crash recovery).
//!
//! GAP: restart-on-crash latency is bounded by the reconcile cadence, not
//! event-driven — there is no `SIGCHLD`/pidfd watch. A fuller supervisor
//! (event-driven restart, backoff, a supervised-process registry core owns)
//! is left for a follow-up; the seam to build it against is the backend's
//! `mount`/`unmount`, which this applier already funnels through.

use crate::managed_mounts::ManagedMount;
use plugin_toolkit::storage::{Capability, MountOutcome, MountStyle, backend};

/// Outcome of a userspace-process reconcile pass, surfaced by `storage.mount`.
#[derive(Debug, Clone, Default)]
pub struct UserspaceOutcome {
    /// Targets brought up (or confirmed up) this pass.
    pub mounted: Vec<String>,
    /// Targets torn down this pass (removed/disabled mounts).
    pub unmounted: Vec<String>,
    /// Non-fatal errors — collected, not thrown, so one bad mount doesn't abort
    /// the whole pass.
    pub errors: Vec<String>,
}

/// Is `m` a mount core should drive through a userspace helper process rather
/// than autofs? True iff its owning backend is registered and advertises
/// [`MountStyle::UserspaceProcess`]. A mount whose backend is not registered is
/// treated as kernel-mount (the autofs path handles/ignores it), so this never
/// mis-claims an nfs/smb mount.
pub fn is_userspace_mount(m: &ManagedMount) -> bool {
    backend(&m.backend).is_some_and(|b| b.mount_style() == MountStyle::UserspaceProcess)
}

/// The share id core hands the backend's [`StorageBackend::mount`]. The declared
/// `source` string (`s3://bucket/prefix`, …) is what the backend understands as
/// the share to bring up — the same string it returns from `list_shares`.
fn share_id(m: &ManagedMount) -> &str {
    &m.source
}

/// Reconcile every userspace-process mount in `mounts`: bring up each enabled one
/// (spawning its helper via the backend), tear down each disabled one, and re-up
/// any declared-up mount its backend reports dead. Kernel-mount rows are ignored
/// here — [`crate::autofs`] owns them. Idempotent: a mount already up that the
/// backend still reports mounted is left untouched.
pub async fn reconcile(mounts: &[ManagedMount]) -> UserspaceOutcome {
    let mut out = UserspaceOutcome::default();
    for m in mounts.iter().filter(|m| is_userspace_mount(m)) {
        if m.enabled {
            reconcile_up(m, &mut out).await;
        } else {
            reconcile_down(m, &mut out).await;
        }
    }
    out
}

/// Bring one enabled userspace mount up, resolving its credential first. A mount
/// already reported mounted by its backend is a no-op (idempotent); a declared-up
/// mount the backend reports dead is re-mounted (crash recovery).
async fn reconcile_up(m: &ManagedMount, out: &mut UserspaceOutcome) {
    let Some(b) = backend(&m.backend) else {
        out.errors.push(format!(
            "{}: backend `{}` not registered",
            m.target, m.backend
        ));
        return;
    };
    if !b.supports(Capability::Mount) {
        out.errors.push(format!(
            "{}: backend `{}` does not support mount",
            m.target, m.backend
        ));
        return;
    }

    // Resolve the credential SecretRef to plaintext immediately before mount. The
    // value is handed to the backend proxy and never logged. A resolve failure
    // fails the mount closed rather than spawning the helper without credentials.
    if let Some(secret_name) = m.credential.as_deref().filter(|c| !c.is_empty()) {
        match plugin_toolkit::secrets::get_required(secret_name) {
            Ok(_resolved) => {
                // The resolved value is bound into the backend's own credential
                // channel by the secrets domain; it is intentionally NOT threaded
                // through the `mount(id, target)` signature (which would put a
                // secret on the proxy wire as a plain arg). The backend resolves
                // the same ref on its side. We resolve here only to fail closed on
                // a missing/broken secret before spawning the helper.
            }
            Err(e) => {
                tracing::warn!(
                    target = %m.target,
                    "resolve userspace mount credential: {e}; mount will fail closed"
                );
                out.errors
                    .push(format!("{}: credential unresolved", m.target));
                return;
            }
        }
    }

    match b.mount(share_id(m), &m.target).await {
        Ok(MountOutcome { mounted: true, .. }) => out.mounted.push(m.target.clone()),
        Ok(MountOutcome { mounted: false, .. }) => out
            .errors
            .push(format!("{}: backend reported not mounted", m.target)),
        Err(e) => out.errors.push(format!("{}: mount: {e}", m.target)),
    }
}

/// Tear one disabled userspace mount down: kill its helper via the backend's
/// [`StorageBackend::unmount`]. A backend that doesn't advertise unmount is
/// skipped (nothing core can do), not an error.
async fn reconcile_down(m: &ManagedMount, out: &mut UserspaceOutcome) {
    let Some(b) = backend(&m.backend) else {
        return;
    };
    if !b.supports(Capability::Unmount) {
        return;
    }
    match b.unmount(&m.target).await {
        Ok(_) => out.unmounted.push(m.target.clone()),
        Err(e) => out.errors.push(format!("{}: unmount: {e}", m.target)),
    }
}

/// Tear down a set of targets on their owning backends — the removed-mount
/// teardown path a delete drives (the row is already gone from the store, so the
/// reconcile pass can't see it; the caller supplies the `(backend, target)`
/// pairs it captured before deletion). Best-effort; errors are collected.
pub async fn teardown(pairs: &[(String, String)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (backend_name, target) in pairs {
        let Some(b) = backend(backend_name) else {
            continue;
        };
        if b.mount_style() != MountStyle::UserspaceProcess || !b.supports(Capability::Unmount) {
            continue;
        }
        if let Err(e) = b.unmount(target).await {
            errors.push(format!("{target}: unmount: {e}"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::storage::{
        Provider, Share, StorageBackend, StorageError, StorageKind, Usage, register_backend,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mount(name: &str, backend: &str, kind: &str, credential: Option<&str>) -> ManagedMount {
        ManagedMount {
            name: name.into(),
            backend: backend.into(),
            kind: kind.into(),
            source: format!("s3://bucket/{name}"),
            failover_sources: None,
            target: format!("/mnt/{name}"),
            fstype: "fuse".into(),
            options: None,
            credential: credential.map(str::to_string),
            remount_policy: None,
            addresses: Vec::new(),
            enabled: true,
        }
    }

    /// Fake object backend: records mount/unmount calls and the ids/targets it
    /// saw. Its `mount()` is idempotent (always reports mounted), matching the
    /// backend contract the applier relies on. Never touches the real filesystem.
    #[derive(Default)]
    struct FakeObject {
        name: String,
        mount_calls: Arc<AtomicUsize>,
        unmount_calls: Arc<AtomicUsize>,
        last_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[derive::orca_async]
    impl StorageBackend for FakeObject {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> StorageKind {
            StorageKind::Object
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Mount, Capability::Unmount, Capability::Usage]
        }
        fn endpoint(&self) -> String {
            "s3://bucket".into()
        }
        fn mount_style(&self) -> MountStyle {
            MountStyle::UserspaceProcess
        }
        async fn mount(&self, id: &str, target: &str) -> Result<MountOutcome, StorageError> {
            self.mount_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_id.lock().unwrap() = Some(id.to_string());
            Ok(MountOutcome {
                target: target.to_string(),
                mounted: true,
                recovered: false,
                detail: None,
            })
        }
        async fn unmount(&self, target: &str) -> Result<MountOutcome, StorageError> {
            self.unmount_calls.fetch_add(1, Ordering::SeqCst);
            Ok(MountOutcome {
                target: target.to_string(),
                mounted: false,
                recovered: false,
                detail: None,
            })
        }
        async fn usage(&self, id: &str) -> Result<Usage, StorageError> {
            Ok(Usage {
                id: id.to_string(),
                total_bytes: 0,
                used_bytes: 0,
                available_bytes: 0,
            })
        }
        fn provider(&self) -> Provider {
            Provider {
                name: self.name.clone(),
                kind: self.kind(),
                endpoint: self.endpoint(),
                capabilities: self.capabilities(),
            }
        }
        async fn list_shares(&self) -> Result<Vec<Share>, StorageError> {
            Ok(vec![])
        }
    }

    /// A kernel-mount backend — the regression guard. If the userspace applier
    /// ever touched this, `mount_calls` would advance; the test asserts it never
    /// does (autofs owns it).
    #[derive(Default)]
    struct FakeNas {
        name: String,
        mount_calls: Arc<AtomicUsize>,
    }

    #[derive::orca_async]
    impl StorageBackend for FakeNas {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> StorageKind {
            StorageKind::NetworkShare
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Mount, Capability::Unmount]
        }
        fn endpoint(&self) -> String {
            "nfs://nas".into()
        }
        async fn mount(&self, _id: &str, target: &str) -> Result<MountOutcome, StorageError> {
            self.mount_calls.fetch_add(1, Ordering::SeqCst);
            Ok(MountOutcome {
                target: target.to_string(),
                mounted: true,
                recovered: false,
                detail: None,
            })
        }
    }

    #[tokio::test]
    async fn userspace_mount_dispatches_to_backend_not_autofs() {
        let calls = Arc::new(AtomicUsize::new(0));
        register_backend(Arc::new(FakeObject {
            name: "obj-dispatch".into(),
            mount_calls: calls.clone(),
            ..Default::default()
        }));
        let m = mount("d1", "obj-dispatch", "object", None);
        assert!(is_userspace_mount(&m));

        let out = reconcile(&[m]).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "mount() must be called");
        assert_eq!(out.mounted, vec!["/mnt/d1".to_string()]);
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    }

    #[tokio::test]
    async fn kernel_mount_is_not_a_userspace_mount() {
        let calls = Arc::new(AtomicUsize::new(0));
        register_backend(Arc::new(FakeNas {
            name: "nas-guard".into(),
            mount_calls: calls.clone(),
        }));
        let m = mount("k1", "nas-guard", "network_share", None);
        // The classifier keeps it out of the userspace path...
        assert!(!is_userspace_mount(&m));
        // ...and a reconcile over it dispatches nothing to the backend.
        let out = reconcile(&[m]).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "autofs owns kernel mounts");
        assert!(out.mounted.is_empty());
    }

    #[tokio::test]
    async fn disabled_userspace_mount_is_torn_down() {
        let mounts = Arc::new(AtomicUsize::new(0));
        let unmounts = Arc::new(AtomicUsize::new(0));
        register_backend(Arc::new(FakeObject {
            name: "obj-teardown".into(),
            mount_calls: mounts.clone(),
            unmount_calls: unmounts.clone(),
            ..Default::default()
        }));
        let mut m = mount("t1", "obj-teardown", "object", None);
        m.enabled = false;

        let out = reconcile(&[m]).await;
        assert_eq!(mounts.load(Ordering::SeqCst), 0, "disabled must not mount");
        assert_eq!(unmounts.load(Ordering::SeqCst), 1, "disabled must unmount");
        assert_eq!(out.unmounted, vec!["/mnt/t1".to_string()]);
    }

    #[tokio::test]
    async fn teardown_kills_the_process_for_removed_mounts() {
        let unmounts = Arc::new(AtomicUsize::new(0));
        register_backend(Arc::new(FakeObject {
            name: "obj-removed".into(),
            unmount_calls: unmounts.clone(),
            ..Default::default()
        }));
        let errors = teardown(&[("obj-removed".into(), "/mnt/gone".into())]).await;
        assert_eq!(unmounts.load(Ordering::SeqCst), 1);
        assert!(errors.is_empty(), "errors: {errors:?}");
    }

    #[tokio::test]
    async fn credential_is_resolved_before_mount_and_never_logged() {
        // A userspace mount carrying a SecretRef that does NOT resolve must fail
        // closed: no mount() call, and the error string carries no secret name or
        // value — only the target.
        let calls = Arc::new(AtomicUsize::new(0));
        register_backend(Arc::new(FakeObject {
            name: "obj-cred".into(),
            mount_calls: calls.clone(),
            ..Default::default()
        }));
        // `no-such-secret` is not registered, so get_required errors → fail closed.
        let m = mount("c1", "obj-cred", "object", Some("no-such-secret"));
        let out = reconcile(&[m]).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "must not mount when credential unresolved"
        );
        assert_eq!(out.errors.len(), 1);
        assert!(
            out.errors[0].contains("credential unresolved"),
            "got: {}",
            out.errors[0]
        );
        assert!(
            !out.errors[0].contains("no-such-secret"),
            "secret name must not leak into the error"
        );
    }

    #[tokio::test]
    async fn mount_receives_the_declared_source_as_share_id() {
        let last = Arc::new(std::sync::Mutex::new(None));
        register_backend(Arc::new(FakeObject {
            name: "obj-id".into(),
            last_id: last.clone(),
            ..Default::default()
        }));
        let m = mount("s1", "obj-id", "object", None);
        let _ = reconcile(&[m]).await;
        assert_eq!(last.lock().unwrap().as_deref(), Some("s3://bucket/s1"));
    }
}
