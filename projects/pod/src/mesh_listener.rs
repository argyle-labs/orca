//! Mesh TCP+mTLS accept loop on `db::ports::mesh_port()` (default 12002).
//!
//! Serves two SNIs on the same port:
//!
//!   * `pod.orca.local` (`POD_SERVER_SAN`) — paired-peer mTLS. Requires a
//!     client cert signed by the mesh CA; CN drives every authorization
//!     decision in `handle_pod_connection`.
//!   * `pod-bootstrap.orca.local` (`POD_BOOTSTRAP_SAN`) — pre-pairing
//!     channel. No client cert: trust is established at the next layer via
//!     pinned bootstrap pubkey + pairing code.
//!
//! The TLS cert resolver and client-cert verifier both read from disk on
//! every handshake so leaf rotation and `pod accept` (writes a fresh mesh
//! CA into place) take effect without a daemon restart — `atomic_write_pem`
//! does a tmp-write + rename so reads only ever see the old or new file.

#![allow(clippy::disallowed_types)] // wire-layer JSON + rustls trait objects

use anyhow::{Context, Result};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::server::WebPkiClientVerifier;
use rustls_pemfile::certs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// Bind the mesh listener and spawn its accept loop. Returns the spawned
/// task's `JoinHandle` so the caller can park it for the daemon's lifetime
/// (dropping the handle does NOT abort the task — `tokio::spawn` detaches —
/// but parking matches the convention used by the sibling `scheduler`,
/// `cert_rotation`, and `roster_sync` spawners).
///
/// Errors surface synchronously when the TLS material on disk is missing or
/// the port is already bound, so the caller can downgrade to a warn without
/// burying the failure inside a detached task.
pub async fn spawn(pki_dir: &Path) -> Result<tokio::task::JoinHandle<()>> {
    let acceptor = build_acceptor(pki_dir).context("build mesh TLS acceptor")?;
    let port = db::ports::mesh_port();
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind mesh listener {addr}"))?;
    utils::mesh_status::set_listening(true);
    let handle = tokio::spawn(serve(listener, acceptor));
    Ok(handle)
}

async fn serve(listener: TcpListener, acceptor: TlsAcceptor) {
    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls = match acceptor.accept(tcp).await {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("[pod] mesh TLS accept failed: {e:#}");
                            return;
                        }
                    };
                    let sni = tls
                        .get_ref()
                        .1
                        .server_name()
                        .map(str::to_string)
                        .unwrap_or_default();

                    // Bootstrap SNI: pre-pair channel, no client cert.
                    // Pairing CANNOT happen without this path.
                    if sni == utils::pki::POD_BOOTSTRAP_SAN {
                        if let Err(e) = crate::handle_pod_bootstrap_connection(tls, peer).await {
                            warn!("[pod] {peer} bootstrap connection error: {e:#}");
                        }
                        return;
                    }

                    if sni == utils::pki::POD_SERVER_SAN {
                        let peer_cn = match extract_peer_cn(&tls) {
                            Ok(cn) => cn,
                            Err(e) => {
                                warn!("[pod] {peer} pod connection lacks valid peer cert: {e:#}");
                                return;
                            }
                        };
                        if let Err(e) = crate::handle_pod_connection(tls, peer_cn, peer).await {
                            warn!("[pod] {peer} pod connection error: {e:#}");
                        }
                        return;
                    }

                    warn!("[pod] {peer} closed connection with unknown SNI: {sni:?}");
                });
            }
            Err(e) => warn!("[pod] mesh accept error: {e}"),
        }
    }
}

fn extract_peer_cn(tls: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>) -> Result<String> {
    let (_, conn) = tls.get_ref();
    let certs = conn
        .peer_certificates()
        .context("peer presented no client cert (mTLS misconfigured?)")?;
    let leaf = certs.first().context("peer cert chain empty")?;
    utils::pki::peer_common_name(leaf.as_ref())
}

// ── TLS acceptor ─────────────────────────────────────────────────────────────

fn build_acceptor(pki_dir: &Path) -> Result<TlsAcceptor> {
    // Eagerly materialize the bootstrap cert+key so the on-disk file exists
    // before the first handshake — the resolver re-reads it per handshake,
    // but the file has to exist on the first one too.
    utils::pki::load_or_init_bootstrap_cert(pki_dir).context("init bootstrap TLS cert")?;

    let resolver = Arc::new(HotReloadResolver {
        pki_dir: pki_dir.to_path_buf(),
    });

    let client_cert_verifier = Arc::new(HotReloadClientVerifier::new(pki_dir)?);

    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_cert_verifier)
        .with_cert_resolver(resolver);

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Cert resolver that reads cert+key PEM from disk on every handshake. This
/// is how seamless leaf-cert rotation works: `atomic_write_pem` does a
/// tmp-write + rename, so a reader sees either the old or the new file but
/// never a half-written one. Cost is microseconds per handshake.
#[derive(Debug)]
struct HotReloadResolver {
    pki_dir: PathBuf,
}

impl HotReloadResolver {
    fn load_pod_server_ck(&self) -> Result<rustls::sign::CertifiedKey> {
        let cert_pem = std::fs::read_to_string(utils::pki::mesh_server_cert_path(&self.pki_dir))
            .context("read mesh server cert")?;
        let key_pem = std::fs::read_to_string(utils::pki::mesh_server_key_path(&self.pki_dir))
            .context("read mesh server key")?;
        Self::build_ck(&cert_pem, &key_pem)
    }

    fn load_bootstrap_ck(&self) -> Result<rustls::sign::CertifiedKey> {
        let cert_pem = std::fs::read_to_string(utils::pki::bootstrap_cert_path(&self.pki_dir))
            .context("read bootstrap cert")?;
        let key_pem = std::fs::read_to_string(utils::pki::bootstrap_key_path(&self.pki_dir))
            .context("read bootstrap key")?;
        Self::build_ck(&cert_pem, &key_pem)
    }

    fn build_ck(cert_pem: &str, key_pem: &str) -> Result<rustls::sign::CertifiedKey> {
        let (chain, key) = utils::pki::parse_cert_and_key(cert_pem, key_pem)?;
        let signing = CryptoProvider::get_default()
            .context("no rustls CryptoProvider installed")?
            .key_provider
            .load_private_key(key)
            .context("load private key")?;
        Ok(rustls::sign::CertifiedKey::new(chain, signing))
    }
}

impl rustls::server::ResolvesServerCert for HotReloadResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let sni = client_hello.server_name()?;
        match sni {
            s if s == utils::pki::POD_SERVER_SAN => self.load_pod_server_ck().ok().map(Arc::new),
            s if s == utils::pki::POD_BOOTSTRAP_SAN => self.load_bootstrap_ck().ok().map(Arc::new),
            _ => None,
        }
    }
}

/// Client-cert verifier whose trust roots track the mesh CA file's mtime.
/// `pod accept` writes a fresh `mesh/ca.cert.pem`; without a hot reload, the
/// daemon's startup snapshot stays stale and rejects the inviter's client
/// cert with `UnknownCA` until restart. `allow_unauthenticated()` lets the
/// bootstrap SNI through (it has no client cert by design) — SNI dispatch
/// in `serve` enforces that the unauthenticated path is bootstrap-only.
#[derive(Debug)]
struct HotReloadClientVerifier {
    pki_dir: PathBuf,
    state: StdMutex<VerifierState>,
}

#[derive(Debug)]
struct VerifierState {
    inner: Arc<dyn rustls::server::danger::ClientCertVerifier>,
    mesh_mtime: Option<std::time::SystemTime>,
    prev_mtime: Option<std::time::SystemTime>,
}

impl HotReloadClientVerifier {
    fn new(pki_dir: &Path) -> Result<Self> {
        let inner = Self::build(pki_dir)?;
        let mesh_mtime = std::fs::metadata(utils::pki::mesh_ca_cert_path(pki_dir))
            .and_then(|m| m.modified())
            .ok();
        let prev_mtime = std::fs::metadata(utils::pki::mesh_ca_previous_cert_path(pki_dir))
            .and_then(|m| m.modified())
            .ok();
        if mesh_mtime.is_some() {
            info!("[pod] mesh CA detected — pod SNI surface active");
        }
        Ok(Self {
            pki_dir: pki_dir.to_path_buf(),
            state: StdMutex::new(VerifierState {
                inner,
                mesh_mtime,
                prev_mtime,
            }),
        })
    }

    fn build(pki_dir: &Path) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>> {
        let mesh = utils::pki::mesh_ca_cert_path(pki_dir);
        let mut roots = rustls::RootCertStore::empty();
        if mesh.exists() {
            let cur_ca = std::fs::read_to_string(&mesh).context("read current mesh CA cert")?;
            for der in certs(&mut cur_ca.as_bytes()) {
                roots.add(der.context("parsing current mesh CA cert")?)?;
            }
            if let Ok(prev) =
                std::fs::read_to_string(utils::pki::mesh_ca_previous_cert_path(pki_dir))
            {
                for der in certs(&mut prev.as_bytes()) {
                    roots.add(der.context("parsing previous mesh CA cert")?)?;
                }
            }
        }
        let v = WebPkiClientVerifier::builder(Arc::new(roots))
            .allow_unauthenticated()
            .build()
            .context("build mesh client cert verifier")?;
        Ok(v)
    }

    fn current(&self) -> Arc<dyn rustls::server::danger::ClientCertVerifier> {
        let mesh_mtime = std::fs::metadata(utils::pki::mesh_ca_cert_path(&self.pki_dir))
            .and_then(|m| m.modified())
            .ok();
        let prev_mtime = std::fs::metadata(utils::pki::mesh_ca_previous_cert_path(&self.pki_dir))
            .and_then(|m| m.modified())
            .ok();
        let mut state = self.state.lock().unwrap();
        if state.mesh_mtime != mesh_mtime || state.prev_mtime != prev_mtime {
            match Self::build(&self.pki_dir) {
                Ok(v) => {
                    state.inner = v;
                    state.mesh_mtime = mesh_mtime;
                    state.prev_mtime = prev_mtime;
                    info!("[pod] reloaded mesh client cert verifier (CA changed on disk)");
                }
                Err(e) => warn!("[pod] mesh CA reload failed: {e:#}; using cached verifier"),
            }
        }
        state.inner.clone()
    }
}

impl rustls::server::danger::ClientCertVerifier for HotReloadClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        self.current()
            .verify_client_cert(end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.current().verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.current().verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.current().supported_verify_schemes()
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }
}
