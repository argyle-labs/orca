//! The `service` backup kind — bridges the existing `ServiceBackend` registry
//! into the unified backup subsystem so service plugins are ONE kind under the
//! single `backup` domain, not a parallel backup system.
//!
//! Each registered `ServiceBackend` (audiobookshelf, sonarr, …) becomes an
//! *instance* of the `service` kind, keyed by its provider name. `backup`
//! delegates to `ServiceBackend::backup`, then stages the produced
//! [`BackupArtifact`] into the store slot: the artifact file (a tar, when the
//! backend uses the `tar` method) is copied into `payload_dir` and a typed
//! sidecar records the artifact so `restore` can reconstruct it and hand it back
//! to `ServiceBackend::restore`.
//!
//! LIMITATION (documented, not a bug): a service backup needs a resolved
//! [`Endpoint`] (runtime, host, token). Those aren't persisted yet — `service.connect`
//! will store them in a follow-up — so this bridge constructs a minimal endpoint
//! (`name` = instance, everything else default). That is sufficient for backends
//! whose backup runs against the local host with no credential (the common
//! docker-on-this-host case) and for PBS/remote once endpoint persistence lands.
//! Non-file artifacts (e.g. a PBS snapshot reference) are recorded in the sidecar
//! but not copied; restore replays them via the backend against the same manager.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use contract::{BoxFuture, ToolCtx};
use plugin_toolkit::service::{self, BackupArtifact, Endpoint};

use super::provider::{BackupOutcome, BackupProvider};

/// Sidecar file (inside the payload dir) holding the produced [`BackupArtifact`].
const SIDECAR: &str = "service-artifact.json";

/// The `service` backup kind.
#[derive(Debug, Default)]
pub struct ServiceKindProvider;

impl ServiceKindProvider {
    pub fn new() -> Self {
        Self
    }
}

impl BackupProvider for ServiceKindProvider {
    fn kind(&self) -> &str {
        "service"
    }

    fn title(&self) -> &str {
        "Service backends"
    }

    /// One instance per registered service backend (its provider name).
    fn instances(&self) -> Vec<String> {
        service::backends()
            .iter()
            .map(|b| b.provider().to_string())
            .collect()
    }

    fn backup<'a>(
        &'a self,
        payload_dir: &'a Path,
        instance: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<BackupOutcome>> {
        Box::pin(async move {
            let backend = service::backend(instance)
                .ok_or_else(|| anyhow!("no service backend `{instance}`"))?;
            let ep = endpoint_for(instance);
            let artifact = backend
                .backup(&ep)
                .await
                .map_err(|e| anyhow!("service backup `{instance}`: {e}"))?;

            let checksum = (!artifact.checksum.is_empty()).then(|| artifact.checksum.clone());
            let note = Some(format!("service `{instance}` artifact {}", artifact.path));
            stage_artifact(payload_dir, &artifact)?;
            Ok(BackupOutcome { checksum, note })
        })
    }

    fn restore<'a>(
        &'a self,
        payload_dir: &'a Path,
        instance: &'a str,
        _ctx: &'a ToolCtx,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let backend = service::backend(instance)
                .ok_or_else(|| anyhow!("no service backend `{instance}`"))?;
            let artifact = load_artifact(payload_dir)?;
            let ep = endpoint_for(instance);
            backend
                .restore(&ep, &artifact)
                .await
                .map_err(|e| anyhow!("service restore `{instance}`: {e}"))?;
            Ok(())
        })
    }
}

/// A minimal endpoint for `instance`. See the module LIMITATION note: once
/// `service.connect` persists endpoints, this resolves the stored descriptor.
fn endpoint_for(instance: &str) -> Endpoint {
    Endpoint {
        name: instance.to_string(),
        ..Default::default()
    }
}

/// Copy the artifact's file into `payload_dir` (when it is a readable local file)
/// and write the typed sidecar. A staged file's sidecar `path` is the bare
/// filename; a non-file artifact keeps its original locator.
fn stage_artifact(payload_dir: &Path, artifact: &BackupArtifact) -> Result<()> {
    let mut stored = artifact.clone();
    let src = Path::new(&artifact.path);
    if src.is_file() {
        let name = src
            .file_name()
            .ok_or_else(|| anyhow!("artifact path has no file name: {}", artifact.path))?;
        let dest = payload_dir.join(name);
        std::fs::copy(src, &dest)
            .with_context(|| format!("stage artifact {} -> {}", src.display(), dest.display()))?;
        stored.path = name.to_string_lossy().into_owned();
    }
    write_sidecar(payload_dir, &stored)
}

/// Reconstruct the artifact for restore: read the sidecar, and if its `path` is a
/// file staged in `payload_dir`, re-anchor it to that absolute path.
fn load_artifact(payload_dir: &Path) -> Result<BackupArtifact> {
    let mut artifact = read_sidecar(payload_dir)?;
    let staged = payload_dir.join(&artifact.path);
    if staged.is_file() {
        artifact.path = staged.to_string_lossy().into_owned();
    }
    Ok(artifact)
}

fn write_sidecar(payload_dir: &Path, artifact: &BackupArtifact) -> Result<()> {
    let path = payload_dir.join(SIDECAR);
    let json = serde_json::to_string_pretty(artifact).context("serialize service artifact")?;
    std::fs::write(&path, json).with_context(|| format!("write sidecar {}", path.display()))
}

fn read_sidecar(payload_dir: &Path) -> Result<BackupArtifact> {
    let path = payload_dir.join(SIDECAR);
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("read sidecar {} (not a service backup?)", path.display()))?;
    serde_json::from_str(&json).with_context(|| format!("parse sidecar {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(path: &str) -> BackupArtifact {
        BackupArtifact {
            service: "sonarr".into(),
            instance: "main".into(),
            path: path.into(),
            timestamp: "20260731-041500".into(),
            size_bytes: 3,
            checksum: "sha256:abc".into(),
        }
    }

    #[test]
    fn kind_and_title() {
        let p = ServiceKindProvider::new();
        assert_eq!(p.kind(), "service");
        assert_eq!(p.title(), "Service backends");
    }

    #[test]
    fn stage_copies_file_and_load_reanchors() {
        let tmp = tempfile::tempdir().unwrap();
        // A real artifact tarball on disk.
        let art_src = tmp.path().join("sonarr-main-123.tar.gz");
        std::fs::write(&art_src, b"tar").unwrap();
        let payload = tmp.path().join("payload");
        std::fs::create_dir_all(&payload).unwrap();

        stage_artifact(&payload, &artifact(art_src.to_str().unwrap())).unwrap();
        // The tarball is copied in, and the sidecar stores the bare filename.
        assert!(payload.join("sonarr-main-123.tar.gz").exists());
        let sidecar = read_sidecar(&payload).unwrap();
        assert_eq!(sidecar.path, "sonarr-main-123.tar.gz");
        assert_eq!(sidecar.checksum, "sha256:abc");

        // Restore re-anchors the bare filename to the staged absolute path.
        let loaded = load_artifact(&payload).unwrap();
        assert_eq!(
            loaded.path,
            payload.join("sonarr-main-123.tar.gz").to_string_lossy()
        );
    }

    #[test]
    fn stage_nonfile_artifact_preserves_locator() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        std::fs::create_dir_all(&payload).unwrap();
        // A PBS-style locator that is not a local file.
        stage_artifact(&payload, &artifact("pbs:ct/100/2026-07-31")).unwrap();
        let loaded = load_artifact(&payload).unwrap();
        assert_eq!(loaded.path, "pbs:ct/100/2026-07-31");
    }

    #[test]
    fn load_without_sidecar_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_artifact(tmp.path()).is_err());
    }

    #[test]
    fn instances_reflect_registry() {
        // No service backends are registered in the unit-test process.
        // Delegation against a real ServiceBackend needs a full backend impl
        // (workload_spec + WorkloadSpec) — impractical here; covered by the
        // service crate's own tests. See module LIMITATION note.
        assert!(ServiceKindProvider::new().instances().is_empty());
    }
}
