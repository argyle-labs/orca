//! PKI: CA generation, node cert issuance, and cert/key loading.
//!
//! All material lives under `~/.orca/pki/`:
//!   ca.cert.pem / ca.key.pem          — root CA (generated once by `orca pki ca-init`)
//!   server/node.cert.pem / node.key.pem — server cert (generated alongside the CA)
//!   plugins/<id>/node.cert.pem / node.key.pem — per-plugin cert
//!
//! Server cert DNS SAN: `core.orca.local`, `localhost`, `127.0.0.1`, `::1`
//! Plugin cert DNS SAN: `<plugin-id>.plugin.orca.local`

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ED25519, SanType,
};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

// ── Algorithm + validity policy ──────────────────────────────────────────────
//
// Every X.509 keypair in orca is Ed25519 (rcgen::PKCS_ED25519). Ed25519 is the
// modern default for TLS 1.3-only fleets: 32-byte keys, 64-byte sigs, fast,
// constant-time, no nonce reuse footguns, and the algorithm carries SHA-512
// internally so we don't pick a hash separately.
//
// Validity windows are pinned explicitly so a future rcgen-default change
// can't silently shorten/lengthen them under us. Short windows + auto-rotation
// (see super::cert_rotation in the server crate) are the security posture:
// a stolen leaf cert is useful for at most PEER_VALIDITY_DAYS.
pub const CA_VALIDITY_DAYS: i64 = 365;
pub const PEER_VALIDITY_DAYS: i64 = 30;
/// Refresh peer certs this many days before expiry. Wide enough to absorb
/// a few missed rotations if the daemon was down.
pub const PEER_REFRESH_THRESHOLD_DAYS: i64 = 7;
/// Bootstrap TLS cert sits on the host's long-lived identity key (mDNS-pinned
/// by every peer), so rotation = re-pair. Keep it long; it's not a leaf in
/// the same sense as the peer certs.
pub const BOOTSTRAP_CERT_VALIDITY_DAYS: i64 = 3650;

/// Build a fresh Ed25519 keypair. Single chokepoint so the algorithm choice
/// is visible in one place. Used for everything except the browser-facing REST
/// server cert (which needs an algorithm browsers will accept — see
/// `gen_keypair_browser_tls`).
fn gen_keypair() -> Result<KeyPair> {
    KeyPair::generate_for(&PKCS_ED25519).context("generate Ed25519 keypair")
}

/// Build a fresh ECDSA P-256 keypair for browser-facing TLS. NSS (Firefox)
/// and BoringSSL (Chrome) reject Ed25519 leaf certs in TLS server auth even
/// though they verify Ed25519 chain signatures fine, so the REST server cert
/// can't share the Ed25519 chokepoint above. P-256 is universally accepted.
fn gen_keypair_browser_tls() -> Result<KeyPair> {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .context("generate ECDSA P-256 keypair for REST server cert")
}

/// Apply `(now-5min, now + days)` to a `CertificateParams`. The 5-minute
/// backdate absorbs reasonable clock skew across the mesh so a freshly-
/// rotated cert isn't rejected by a peer whose clock is slightly ahead.
fn set_validity_days(params: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(days);
}

/// Capability class encoded in the plugin cert's Subject OU field.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    General,
    Sensitive,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::General => "general",
            Capability::Sensitive => "sensitive",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Capability {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "general" => Ok(Capability::General),
            "sensitive" => Ok(Capability::Sensitive),
            other => anyhow::bail!("unknown capability: {other}"),
        }
    }
}

/// PEM-encoded cert + key bundle for a node (server or plugin).
#[derive(Debug, Clone)]
pub struct NodeBundle {
    pub cert_pem: String,
    pub key_pem: String,
    /// CA cert so the recipient can verify the server cert.
    pub ca_cert_pem: String,
}

// ── Pod / mesh-CA file paths ─────────────────────────────────────────────────
//
// Pod material lives in a separate subtree from plugin material so the two
// trust contexts can never accidentally cross-contaminate, even when both
// listen on the same port via SNI:
//
//   <pki_dir>/mesh/ca.cert.pem         — pod CA cert (replicated to every peer)
//   <pki_dir>/mesh/ca.key.pem          — pod CA private key (ONLY on secure hosts)
//   <pki_dir>/mesh/server/node.{cert,key}.pem — this host's pod-server cert (SAN=pod.orca.local)
//   <pki_dir>/mesh/client/node.{cert,key}.pem — this host's pod-client cert (used outbound to peers)
//
// Server cert SAN: `pod.orca.local` (the SNI the client sends to reach this surface).
// Client cert CN:  `<hostname>` so server-side handlers can identify the caller.

pub fn mesh_dir(pki_dir: &Path) -> PathBuf {
    pki_dir.join("mesh")
}
pub fn mesh_ca_cert_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("ca.cert.pem")
}
pub fn mesh_ca_key_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("ca.key.pem")
}
pub fn mesh_server_cert_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("server/node.cert.pem")
}
pub fn mesh_server_key_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("server/node.key.pem")
}
pub fn mesh_client_cert_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("client/node.cert.pem")
}
pub fn mesh_client_key_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("client/node.key.pem")
}

// Two-slot CA rotation. During an overlap window after `pod ca-rotate`, both
// the current CA (`ca.cert.pem`) and the previous CA (`ca.previous.cert.pem`)
// are in the trust store, so certs signed by EITHER are accepted. New certs
// (auto-rotation refreshes, new joiners) are issued under the current CA.
// `pod_self.ca_previous_expires_at` is the deadline at which the previous
// slot is dropped from disk + trust.
pub fn mesh_ca_previous_cert_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("ca.previous.cert.pem")
}
pub fn mesh_ca_previous_key_path(pki_dir: &Path) -> PathBuf {
    mesh_dir(pki_dir).join("ca.previous.key.pem")
}
pub fn has_mesh_ca_previous(pki_dir: &Path) -> bool {
    mesh_ca_previous_cert_path(pki_dir).exists()
}

pub const POD_SERVER_SAN: &str = "pod.orca.local";

// ── Pod / mesh-CA init + issuance ────────────────────────────────────────────

/// Founder bootstrap. Creates a fresh mesh CA + this host's pod server cert +
/// this host's pod client cert. Idempotent: re-running with an existing CA is
/// a no-op (returns Ok without regenerating).
///
/// `host_cn` is the CN baked into both the server and client certs — typically
/// the host's `gethostname()`. Used by peers to identify the caller.
pub fn init_mesh_ca(pki_dir: &Path, host_cn: &str) -> Result<()> {
    if mesh_ca_cert_path(pki_dir).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(mesh_dir(pki_dir))
        .with_context(|| format!("create mesh dir {}", mesh_dir(pki_dir).display()))?;

    let ca_key = gen_keypair()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    set_validity_days(&mut ca_params, CA_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-mesh-ca");
        dn.push(DnType::OrganizationName, "orca");
        ca_params.distinguished_name = dn;
    }
    let ca_cert = ca_params.self_signed(&ca_key)?;
    write_pem(mesh_ca_cert_path(pki_dir), &ca_cert.pem())?;
    write_pem(mesh_ca_key_path(pki_dir), &ca_key.serialize_pem())?;

    let issuer = Issuer::new(ca_params, ca_key);
    issue_mesh_server_cert(pki_dir, &issuer)?;
    issue_mesh_client_cert(pki_dir, &issuer, host_cn)?;
    Ok(())
}

/// Re-issue this host's mesh server cert from the mesh CA. Used by `pod init`
/// and by the join flow once the peer cert lands.
fn issue_mesh_server_cert(pki_dir: &Path, issuer: &Issuer<'_, KeyPair>) -> Result<()> {
    let key = gen_keypair()?;
    let mut params = CertificateParams::new(vec![POD_SERVER_SAN.to_string()])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    set_validity_days(&mut params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-pod-server");
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "pod-server");
        params.distinguished_name = dn;
    }
    let cert = params.signed_by(&key, issuer)?;
    let server_dir = mesh_dir(pki_dir).join("server");
    std::fs::create_dir_all(&server_dir)?;
    write_pem(mesh_server_cert_path(pki_dir), &cert.pem())?;
    write_pem(mesh_server_key_path(pki_dir), &key.serialize_pem())?;
    Ok(())
}

/// Issue this host's mesh client cert from the mesh CA. `host_cn` becomes the
/// Subject CN so server-side handlers can identify the caller.
fn issue_mesh_client_cert(
    pki_dir: &Path,
    issuer: &Issuer<'_, KeyPair>,
    host_cn: &str,
) -> Result<()> {
    let key = gen_keypair()?;
    let mut params = CertificateParams::new(vec![format!("{host_cn}.pod.orca.local")])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    set_validity_days(&mut params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host_cn.to_string());
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "pod-client");
        params.distinguished_name = dn;
    }
    let cert = params.signed_by(&key, issuer)?;
    let client_dir = mesh_dir(pki_dir).join("client");
    std::fs::create_dir_all(&client_dir)?;
    write_pem(mesh_client_cert_path(pki_dir), &cert.pem())?;
    write_pem(mesh_client_key_path(pki_dir), &key.serialize_pem())?;
    Ok(())
}

/// Load this host's pod server bundle (cert + key + mesh CA cert).
pub fn load_mesh_server(pki_dir: &Path) -> Result<NodeBundle> {
    Ok(NodeBundle {
        cert_pem: std::fs::read_to_string(mesh_server_cert_path(pki_dir))
            .context("mesh server cert not found — run `orca pod init`")?,
        key_pem: std::fs::read_to_string(mesh_server_key_path(pki_dir))
            .context("mesh server key not found — run `orca pod init`")?,
        ca_cert_pem: std::fs::read_to_string(mesh_ca_cert_path(pki_dir))
            .context("mesh CA cert not found — run `orca pod init`")?,
    })
}

/// Load this host's pod client bundle (used to dial peers).
pub fn load_mesh_client(pki_dir: &Path) -> Result<NodeBundle> {
    Ok(NodeBundle {
        cert_pem: std::fs::read_to_string(mesh_client_cert_path(pki_dir))
            .context("mesh client cert not found — run `orca pod init`")?,
        key_pem: std::fs::read_to_string(mesh_client_key_path(pki_dir))
            .context("mesh client key not found — run `orca pod init`")?,
        ca_cert_pem: std::fs::read_to_string(mesh_ca_cert_path(pki_dir))
            .context("mesh CA cert not found — run `orca pod init`")?,
    })
}

/// True if this host has the mesh CA private key — i.e. can sign new peer
/// certs. v1: only the founder. v2: every secure host after CA replication.
pub fn has_mesh_ca_key(pki_dir: &Path) -> bool {
    mesh_ca_key_path(pki_dir).exists()
}

// ── CSR-based peer enrollment ────────────────────────────────────────────────

/// Role of the cert being requested by a joining peer. Determines SAN / OU /
/// EKU after the founder enforces naming policy.
#[derive(Debug, Clone, Copy)]
pub enum PeerRole {
    /// Outbound client cert — used by the peer to dial other hosts.
    Client,
    /// Inbound server cert — bound to SNI `pod.orca.local`.
    Server,
}

/// Joiner side. Generate a fresh keypair locally, build a CSR for the given
/// role, and return `(csr_pem, key_pem)`. The private key never leaves this
/// host; only `csr_pem` is sent to the inviting peer.
pub fn build_peer_csr(peer_cn: &str, role: PeerRole) -> Result<(String, String)> {
    let key = gen_keypair()?;
    let san = match role {
        PeerRole::Client => format!("{peer_cn}.pod.orca.local"),
        PeerRole::Server => POD_SERVER_SAN.to_string(),
    };
    let mut params = CertificateParams::new(vec![san])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![match role {
        PeerRole::Client => ExtendedKeyUsagePurpose::ClientAuth,
        PeerRole::Server => ExtendedKeyUsagePurpose::ServerAuth,
    }];
    let mut dn = DistinguishedName::new();
    dn.push(
        DnType::CommonName,
        match role {
            PeerRole::Client => peer_cn.to_string(),
            PeerRole::Server => "orca-pod-server".to_string(),
        },
    );
    dn.push(DnType::OrganizationName, "orca");
    dn.push(
        DnType::OrganizationalUnitName,
        match role {
            PeerRole::Client => "pod-client",
            PeerRole::Server => "pod-server",
        },
    );
    params.distinguished_name = dn;

    let csr = params.serialize_request(&key)?;
    Ok((csr.pem()?, key.serialize_pem()))
}

/// Founder side. Parse a CSR from a joining peer, enforce naming policy
/// (overrides whatever the joiner put in the CSR — joiner can't lie about
/// its CN), and sign with the mesh CA. Returns the signed cert PEM and the
/// mesh CA cert PEM (so the joiner can build its trust store).
pub fn sign_peer_csr(
    pki_dir: &Path,
    csr_pem: &str,
    peer_cn: &str,
    role: PeerRole,
) -> Result<(String, String)> {
    use rcgen::CertificateSigningRequestParams;

    anyhow::ensure!(
        has_mesh_ca_key(pki_dir),
        "this host does not have the mesh CA private key — cannot sign peer CSRs"
    );

    let ca_cert_pem =
        std::fs::read_to_string(mesh_ca_cert_path(pki_dir)).context("read mesh CA cert")?;
    let ca_key_pem =
        std::fs::read_to_string(mesh_ca_key_path(pki_dir)).context("read mesh CA key")?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key)?;

    let mut csr =
        CertificateSigningRequestParams::from_pem(csr_pem).context("parse / verify peer CSR")?;

    // Enforce naming policy: rewrite SAN, DN, EKU regardless of what the
    // joiner asked for. Joiner-controlled fields are not trusted.
    let san = match role {
        PeerRole::Client => format!("{peer_cn}.pod.orca.local"),
        PeerRole::Server => POD_SERVER_SAN.to_string(),
    };
    csr.params.subject_alt_names.clear();
    csr.params = {
        let mut p = CertificateParams::new(vec![san])?;
        p.is_ca = IsCa::NoCa;
        p.extended_key_usages = vec![match role {
            PeerRole::Client => ExtendedKeyUsagePurpose::ClientAuth,
            PeerRole::Server => ExtendedKeyUsagePurpose::ServerAuth,
        }];
        set_validity_days(&mut p, PEER_VALIDITY_DAYS);
        let mut dn = DistinguishedName::new();
        dn.push(
            DnType::CommonName,
            match role {
                PeerRole::Client => peer_cn.to_string(),
                PeerRole::Server => "orca-pod-server".to_string(),
            },
        );
        dn.push(DnType::OrganizationName, "orca");
        dn.push(
            DnType::OrganizationalUnitName,
            match role {
                PeerRole::Client => "pod-client",
                PeerRole::Server => "pod-server",
            },
        );
        p.distinguished_name = dn;
        p
    };

    let cert = csr.signed_by(&issuer)?;
    Ok((cert.pem(), ca_cert_pem))
}

// ── CA-key replication ───────────────────────────────────────────────────────

/// Export the mesh CA cert+key as PEM strings, for transfer to a peer that's
/// just become mutually trusted. Caller is responsible for moving these over
/// an already-authenticated mTLS channel and never persisting them in transit.
pub fn export_mesh_ca_keypair(pki_dir: &Path) -> Result<(String, String)> {
    let cert =
        std::fs::read_to_string(mesh_ca_cert_path(pki_dir)).context("export: read mesh CA cert")?;
    let key = std::fs::read_to_string(mesh_ca_key_path(pki_dir))
        .context("export: read mesh CA key — this host is not founder-equivalent")?;
    Ok((cert, key))
}

/// Import a mesh CA keypair received from a trusted peer. Verifies the cert
/// PEM matches what we already have on disk (so a malicious peer can't
/// substitute a different CA), then writes the key. Idempotent if the key
/// already exists with matching content.
pub fn import_mesh_ca_keypair(pki_dir: &Path, cert_pem: &str, key_pem: &str) -> Result<()> {
    let existing_cert = std::fs::read_to_string(mesh_ca_cert_path(pki_dir))
        .context("import: read local mesh CA cert (run `orca pod join` first)")?;
    anyhow::ensure!(
        existing_cert.trim() == cert_pem.trim(),
        "imported CA cert does not match local mesh CA — refusing to install foreign key"
    );
    // Sanity: verify the imported key actually signs against this cert.
    let key = KeyPair::from_pem(key_pem).context("imported CA key is not valid PEM")?;
    Issuer::from_ca_cert_pem(cert_pem, key).context("imported key does not match CA cert")?;
    write_pem(mesh_ca_key_path(pki_dir), key_pem)?;
    Ok(())
}

// ── Cert rotation primitives ─────────────────────────────────────────────────
//
// Short cert lifetimes (PEER_VALIDITY_DAYS = 30) require seamless rotation.
// The two pieces that make rotation safe:
//
//   * `atomic_write_pem` — write-to-tmp + rename(2). Readers (TLS resolver,
//     outbound dial path) either see the old file or the new file, never a
//     half-written one.
//   * `should_rotate(cert_pem, threshold_days)` — parses the cert, returns
//     true when `not_after - now < threshold_days`. The rotation task in
//     server::pod::cert_rotation polls every cert and reissues when this
//     fires.
//
// The TLS resolver in plugin_host reads certs from disk on every handshake,
// so an atomic file swap is enough — no in-process cache to invalidate.

/// Days remaining until `cert_pem` expires. Negative for already-expired.
pub fn cert_days_remaining(cert_pem: &str) -> Result<i64> {
    use rustls_pemfile::certs;
    let mut reader = cert_pem.as_bytes();
    let der = certs(&mut reader)
        .next()
        .context("no certificate in PEM")?
        .context("parse cert DER")?;
    let (_, parsed) =
        x509_parser::parse_x509_certificate(der.as_ref()).context("parse cert for not_after")?;
    let not_after_secs = parsed.validity().not_after.timestamp();
    let now_secs = time::OffsetDateTime::now_utc().unix_timestamp();
    Ok((not_after_secs - now_secs) / 86_400)
}

/// True iff the cert is within `threshold_days` of expiring (or already past).
pub fn should_rotate(cert_pem: &str, threshold_days: i64) -> Result<bool> {
    Ok(cert_days_remaining(cert_pem)? <= threshold_days)
}

/// Atomic file write: writes to `<path>.tmp`, fsyncs, renames over `<path>`.
/// Readers of `path` see either the old content or the new content; rename
/// is atomic at the namespace-entry level on POSIX. Restricts key files to
/// 0o600 to match `write_pem`.
pub fn atomic_write_pem(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent {}", parent.display()))?;
    }
    let tmp = path.with_extension("pem.tmp");
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("open tmp {}", tmp.display()))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    if path.to_string_lossy().contains(".key.") {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Re-issue this host's mesh server cert against the local mesh CA. Requires
/// `has_mesh_ca_key`. New keypair, new SAN/CN/EKU material (identical to
/// init-time issuance). Atomic on disk.
pub fn reissue_mesh_server_cert(pki_dir: &Path) -> Result<()> {
    anyhow::ensure!(
        has_mesh_ca_key(pki_dir),
        "reissue: this host does not have the mesh CA private key"
    );
    let ca_cert_pem = std::fs::read_to_string(mesh_ca_cert_path(pki_dir))?;
    let ca_key_pem = std::fs::read_to_string(mesh_ca_key_path(pki_dir))?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key)?;

    let key = gen_keypair()?;
    let mut params = CertificateParams::new(vec![POD_SERVER_SAN.to_string()])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    set_validity_days(&mut params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-pod-server");
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "pod-server");
        params.distinguished_name = dn;
    }
    let cert = params.signed_by(&key, &issuer)?;
    atomic_write_pem(&mesh_server_cert_path(pki_dir), &cert.pem())?;
    atomic_write_pem(&mesh_server_key_path(pki_dir), &key.serialize_pem())?;
    Ok(())
}

/// Re-issue this host's mesh client cert against the local mesh CA. Same
/// preconditions as `reissue_mesh_server_cert`.
pub fn reissue_mesh_client_cert(pki_dir: &Path, host_cn: &str) -> Result<()> {
    anyhow::ensure!(
        has_mesh_ca_key(pki_dir),
        "reissue: this host does not have the mesh CA private key"
    );
    let ca_cert_pem = std::fs::read_to_string(mesh_ca_cert_path(pki_dir))?;
    let ca_key_pem = std::fs::read_to_string(mesh_ca_key_path(pki_dir))?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key)?;

    let key = gen_keypair()?;
    let mut params = CertificateParams::new(vec![format!("{host_cn}.pod.orca.local")])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    set_validity_days(&mut params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host_cn.to_string());
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "pod-client");
        params.distinguished_name = dn;
    }
    let cert = params.signed_by(&key, &issuer)?;
    atomic_write_pem(&mesh_client_cert_path(pki_dir), &cert.pem())?;
    atomic_write_pem(&mesh_client_key_path(pki_dir), &key.serialize_pem())?;
    Ok(())
}

// ── CA rotation (two-slot with overlap) ──────────────────────────────────────

/// Rotate the mesh CA. Existing `ca.cert.pem`/`ca.key.pem` move into the
/// `previous` slot; a new CA keypair is generated and written to the
/// `current` slot. Existing peer certs (signed by the now-previous CA) keep
/// validating until the previous slot is dropped via `drop_mesh_ca_previous`.
///
/// Requires `has_mesh_ca_key` — only secure hosts can rotate. Caller is
/// responsible for replicating both slots to mutual-secure peers and
/// recording the overlap expiry in DB.
pub fn rotate_mesh_ca(pki_dir: &Path) -> Result<()> {
    anyhow::ensure!(
        has_mesh_ca_key(pki_dir),
        "ca-rotate: this host does not have the mesh CA private key"
    );
    let cur_cert = mesh_ca_cert_path(pki_dir);
    let cur_key = mesh_ca_key_path(pki_dir);
    let prev_cert = mesh_ca_previous_cert_path(pki_dir);
    let prev_key = mesh_ca_previous_key_path(pki_dir);

    // Slide current → previous (overwrites any older previous slot).
    let cur_cert_pem = std::fs::read_to_string(&cur_cert).context("read current CA cert")?;
    let cur_key_pem = std::fs::read_to_string(&cur_key).context("read current CA key")?;
    atomic_write_pem(&prev_cert, &cur_cert_pem)?;
    atomic_write_pem(&prev_key, &cur_key_pem)?;

    // Generate fresh current.
    let new_key = gen_keypair()?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    set_validity_days(&mut params, CA_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-mesh-ca");
        dn.push(DnType::OrganizationName, "orca");
        params.distinguished_name = dn;
    }
    let new_cert = params.self_signed(&new_key)?;
    atomic_write_pem(&cur_cert, &new_cert.pem())?;
    atomic_write_pem(&cur_key, &new_key.serialize_pem())?;
    Ok(())
}

/// Drop the previous CA slot. Called once the overlap window expires.
/// Idempotent: missing files are not an error.
pub fn drop_mesh_ca_previous(pki_dir: &Path) -> Result<()> {
    for p in [
        mesh_ca_previous_cert_path(pki_dir),
        mesh_ca_previous_key_path(pki_dir),
    ] {
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        }
    }
    Ok(())
}

/// Install both CA slots from a peer (CA replication that's two-slot-aware).
/// `previous_*` may be None when the source pod has never rotated. Both
/// slots are verified before write (cert+key match).
pub fn import_mesh_ca_state(
    pki_dir: &Path,
    current_cert_pem: &str,
    current_key_pem: &str,
    previous_cert_pem: Option<&str>,
    previous_key_pem: Option<&str>,
) -> Result<()> {
    // Verify current.
    let cur_key =
        KeyPair::from_pem(current_key_pem).context("imported current CA key is not valid PEM")?;
    Issuer::from_ca_cert_pem(current_cert_pem, cur_key)
        .context("imported current key does not match current cert")?;
    // Verify previous if present.
    if let (Some(c), Some(k)) = (previous_cert_pem, previous_key_pem) {
        let prev_key = KeyPair::from_pem(k).context("imported previous CA key is not valid PEM")?;
        Issuer::from_ca_cert_pem(c, prev_key)
            .context("imported previous key does not match previous cert")?;
    }

    atomic_write_pem(&mesh_ca_cert_path(pki_dir), current_cert_pem)?;
    atomic_write_pem(&mesh_ca_key_path(pki_dir), current_key_pem)?;
    if let (Some(c), Some(k)) = (previous_cert_pem, previous_key_pem) {
        atomic_write_pem(&mesh_ca_previous_cert_path(pki_dir), c)?;
        atomic_write_pem(&mesh_ca_previous_key_path(pki_dir), k)?;
    }
    Ok(())
}

/// Build a `RootCertStore` containing every CA cert PEM in the iterator.
/// Used by the pod trust path to span the overlap window where both
/// current and previous CAs validate inbound certs.
pub fn ca_root_store_multi<'a, I: IntoIterator<Item = &'a str>>(
    ca_pems: I,
) -> Result<rustls::RootCertStore> {
    use rustls_pemfile::certs;
    let mut store = rustls::RootCertStore::empty();
    for pem in ca_pems {
        for der in certs(&mut pem.as_bytes()) {
            store.add(der.context("parse CA cert")?)?;
        }
    }
    Ok(store)
}

/// Joiner side of a refresh: build fresh CSRs for both roles, return as
/// `(client_csr_pem, client_key_pem, server_csr_pem, server_key_pem)`. The
/// pod/refresh-cert handler on a peer with the mesh CA key signs them and
/// returns the certs.
pub fn build_refresh_csrs(host_cn: &str) -> Result<(String, String, String, String)> {
    let (csr_client, key_client) = build_peer_csr(host_cn, PeerRole::Client)?;
    let (csr_server, key_server) = build_peer_csr(host_cn, PeerRole::Server)?;
    Ok((csr_client, key_client, csr_server, key_server))
}

/// Atomically install refreshed certs received from a peer. Caller passes
/// the certs from the pod/refresh-cert response plus the locally-generated
/// keys (from `build_refresh_csrs`).
pub fn install_refreshed_peer_certs(
    pki_dir: &Path,
    client_cert_pem: &str,
    client_key_pem: &str,
    server_cert_pem: &str,
    server_key_pem: &str,
) -> Result<()> {
    atomic_write_pem(&mesh_client_cert_path(pki_dir), client_cert_pem)?;
    atomic_write_pem(&mesh_client_key_path(pki_dir), client_key_pem)?;
    atomic_write_pem(&mesh_server_cert_path(pki_dir), server_cert_pem)?;
    atomic_write_pem(&mesh_server_key_path(pki_dir), server_key_pem)?;
    Ok(())
}

// ── File paths (plugin / legacy) ─────────────────────────────────────────────

pub fn ca_cert_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("ca.cert.pem")
}
pub fn ca_key_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("ca.key.pem")
}
pub fn server_cert_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("server/node.cert.pem")
}
pub fn server_key_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("server/node.key.pem")
}
pub fn plugin_cert_path(pki_dir: &Path, plugin_id: &str) -> PathBuf {
    pki_dir.join(format!("plugins/{plugin_id}/node.cert.pem"))
}
pub fn plugin_key_path(pki_dir: &Path, plugin_id: &str) -> PathBuf {
    pki_dir.join(format!("plugins/{plugin_id}/node.key.pem"))
}

/// CLI client cert — local-host identity for calling the REST API on `:12000`
/// over mTLS. CN is `cli.<host_cn>`; signed by the core CA at `ca.cert.pem`.
pub fn cli_client_cert_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("client.cert.pem")
}
pub fn cli_client_key_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("client.key.pem")
}

// ── REST server cert helpers ──────────────────────────────────────────────────

/// SANs for the REST server cert. Includes `localhost` and loopback IPs so that
/// browsers accessing `https://localhost:<port>` get a valid hostname match —
/// required for browsers to store cookies on connections with self-signed CAs.
fn rest_server_sans() -> Vec<SanType> {
    vec![
        SanType::DnsName("core.orca.local".try_into().expect("valid IA5")),
        SanType::DnsName("localhost".try_into().expect("valid IA5")),
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ]
}

/// True if the REST server cert already has `localhost` as a DNS SAN.
/// Used at daemon startup to detect pre-upgrade certs that need re-issuance.
pub fn rest_server_cert_has_localhost_san(cert_pem: &str) -> bool {
    use rustls_pemfile::certs;
    let mut reader = cert_pem.as_bytes();
    let der = match certs(&mut reader).next() {
        Some(Ok(d)) => d,
        _ => return false,
    };
    let (_, parsed) = match x509_parser::parse_x509_certificate(der.as_ref()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|ext| {
            ext.value.general_names.iter().any(|n| {
                matches!(
                    n,
                    x509_parser::extensions::GeneralName::DNSName("localhost")
                )
            })
        })
        .unwrap_or(false)
}

/// True if the REST server cert's public key is browser-compatible (currently
/// ECDSA P-256 or RSA). Pre-rc.9 certs used Ed25519 leaf keys which Firefox
/// and Chrome reject in TLS server auth (handshake fails with no override).
/// Detected via the SPKI algorithm OID — Ed25519 is `1.3.101.112`.
pub fn rest_server_cert_is_browser_compatible(cert_pem: &str) -> bool {
    use rustls_pemfile::certs;
    let mut reader = cert_pem.as_bytes();
    let der = match certs(&mut reader).next() {
        Some(Ok(d)) => d,
        _ => return false,
    };
    let (_, parsed) = match x509_parser::parse_x509_certificate(der.as_ref()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Ed25519 OID 1.3.101.112 — anything else (ECDSA, RSA) is fine for browsers.
    let oid = parsed.public_key().algorithm.algorithm.to_string();
    oid != "1.3.101.112"
}

/// Re-issue the REST server cert under the existing CA. Called when the cert
/// lacks the `localhost` SAN (pre-upgrade cert) so the new SANs take effect
/// without requiring a full re-init.
pub fn refresh_rest_server_cert(pki_dir: &Path) -> Result<()> {
    let ca_cert_pem = std::fs::read_to_string(ca_cert_path(pki_dir))
        .context("CA cert not found; run `orca install` to initialize PKI")?;
    let ca_key_pem = std::fs::read_to_string(ca_key_path(pki_dir))
        .context("CA key not found; run `orca install` to initialize PKI")?;
    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parse CA key")?;
    let issuer =
        Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key).context("build CA issuer for refresh")?;
    issue_rest_server_cert(pki_dir, &issuer)
}

/// Issue (or re-issue) the REST server cert under `issuer`. Atomic on disk.
fn issue_rest_server_cert(pki_dir: &Path, issuer: &Issuer<'_, KeyPair>) -> Result<()> {
    let server_key = gen_keypair_browser_tls()?;
    let mut server_params = CertificateParams::default();
    server_params.subject_alt_names = rest_server_sans();
    server_params.is_ca = IsCa::NoCa;
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    set_validity_days(&mut server_params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-core");
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "server");
        server_params.distinguished_name = dn;
    }
    let server_cert = server_params
        .signed_by(&server_key, issuer)
        .context("sign REST server cert")?;
    let server_dir = pki_dir.join("server");
    std::fs::create_dir_all(&server_dir)?;
    atomic_write_pem(&server_cert_path(pki_dir), &server_cert.pem())?;
    atomic_write_pem(&server_key_path(pki_dir), &server_key.serialize_pem())?;
    Ok(())
}

// ── Init ──────────────────────────────────────────────────────────────────────

/// Generate and persist the CA + server cert. Safe to call multiple times —
/// skips if `ca.cert.pem` already exists.
pub fn init(pki_dir: &Path) -> Result<()> {
    if ca_cert_path(pki_dir).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(pki_dir)
        .with_context(|| format!("create pki dir {}", pki_dir.display()))?;

    // CA
    let ca_key = gen_keypair()?;
    let ca_key_pem = ca_key.serialize_pem();
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    set_validity_days(&mut ca_params, CA_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-ca");
        dn.push(DnType::OrganizationName, "orca");
        ca_params.distinguished_name = dn;
    }
    let ca_cert = ca_params.self_signed(&ca_key)?;

    write_pem(ca_cert_path(pki_dir), &ca_cert.pem())?;
    write_pem(ca_key_path(pki_dir), &ca_key_pem)?;

    // rcgen 0.14 split signing into a separate Issuer that owns the key.
    let issuer = Issuer::new(ca_params, ca_key);

    issue_rest_server_cert(pki_dir, &issuer)?;

    Ok(())
}

// ── Issue plugin cert ─────────────────────────────────────────────────────────

/// Issue a cert for `plugin_id` signed by the CA. Errors if the CA does not
/// exist — caller must run `init` first.
pub fn issue(pki_dir: &Path, plugin_id: &str, capability: Capability) -> Result<NodeBundle> {
    let ca_cert_pem = std::fs::read_to_string(ca_cert_path(pki_dir))
        .context("CA cert not found — run `orca pki ca-init` first")?;
    let ca_key_pem = std::fs::read_to_string(ca_key_path(pki_dir))
        .context("CA key not found — run `orca pki ca-init` first")?;

    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key)?;

    let dns_san = format!("{plugin_id}.plugin.orca.local");
    let plugin_key = gen_keypair()?;
    let mut params = CertificateParams::new(vec![dns_san])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    set_validity_days(&mut params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, plugin_id);
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, capability.as_str());
        params.distinguished_name = dn;
    }
    let plugin_cert = params.signed_by(&plugin_key, &issuer)?;

    // Persist
    let plugin_dir = pki_dir.join(format!("plugins/{plugin_id}"));
    std::fs::create_dir_all(&plugin_dir)?;
    write_pem(plugin_cert_path(pki_dir, plugin_id), &plugin_cert.pem())?;
    write_pem(
        plugin_key_path(pki_dir, plugin_id),
        &plugin_key.serialize_pem(),
    )?;

    Ok(NodeBundle {
        cert_pem: plugin_cert.pem(),
        key_pem: plugin_key.serialize_pem(),
        ca_cert_pem,
    })
}

// ── Issue CLI client cert ────────────────────────────────────────────────────

/// Issue this host's CLI client cert from the core CA. CN = `cli.<host_cn>`;
/// SAN = `cli.<host_cn>.orca.local`; EKU = ClientAuth. Idempotent: returns the
/// existing bundle if `client.cert.pem` is already present.
///
/// Consumed by the orca CLI to authenticate to the local REST API (`:12000`)
/// over mTLS. NOT used for pod federation — that uses `mesh/client/*` under
/// the mesh CA.
pub fn issue_cli_client_cert(pki_dir: &Path, host_cn: &str) -> Result<NodeBundle> {
    let ca_cert_pem = std::fs::read_to_string(ca_cert_path(pki_dir))
        .context("CA cert not found — run `orca install` (which runs utils::pki::init) first")?;

    if cli_client_cert_path(pki_dir).exists() && cli_client_key_path(pki_dir).exists() {
        return Ok(NodeBundle {
            cert_pem: std::fs::read_to_string(cli_client_cert_path(pki_dir))?,
            key_pem: std::fs::read_to_string(cli_client_key_path(pki_dir))?,
            ca_cert_pem,
        });
    }

    let ca_key_pem = std::fs::read_to_string(ca_key_path(pki_dir))
        .context("CA key not found — this host cannot sign new CLI client certs")?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key)?;

    let key = gen_keypair()?;
    let san = format!("cli.{host_cn}.orca.local");
    let mut params = CertificateParams::new(vec![san])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    set_validity_days(&mut params, PEER_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, format!("cli.{host_cn}"));
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "cli");
        params.distinguished_name = dn;
    }
    let cert = params.signed_by(&key, &issuer)?;

    write_pem(cli_client_cert_path(pki_dir), &cert.pem())?;
    write_pem(cli_client_key_path(pki_dir), &key.serialize_pem())?;

    Ok(NodeBundle {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        ca_cert_pem,
    })
}

/// Load the CLI client cert bundle if it exists. Returns `None` if either
/// `client.cert.pem` or `client.key.pem` is missing — caller decides whether
/// to attempt issuance or fall back to bearer-token auth.
pub fn load_cli_client(pki_dir: &Path) -> Option<NodeBundle> {
    let cert_pem = std::fs::read_to_string(cli_client_cert_path(pki_dir)).ok()?;
    let key_pem = std::fs::read_to_string(cli_client_key_path(pki_dir)).ok()?;
    let ca_cert_pem = std::fs::read_to_string(ca_cert_path(pki_dir)).ok()?;
    Some(NodeBundle {
        cert_pem,
        key_pem,
        ca_cert_pem,
    })
}

// ── Load ──────────────────────────────────────────────────────────────────────

/// Load the server's TLS material (cert chain + key + CA cert) from disk.
pub fn load_server(pki_dir: &Path) -> Result<NodeBundle> {
    Ok(NodeBundle {
        cert_pem: std::fs::read_to_string(server_cert_path(pki_dir))
            .context("server cert not found — run `orca pki ca-init`")?,
        key_pem: std::fs::read_to_string(server_key_path(pki_dir))
            .context("server key not found — run `orca pki ca-init`")?,
        ca_cert_pem: std::fs::read_to_string(ca_cert_path(pki_dir))
            .context("CA cert not found — run `orca pki ca-init`")?,
    })
}

/// Load a plugin's TLS material from disk.
pub fn load_plugin(pki_dir: &Path, plugin_id: &str) -> Result<NodeBundle> {
    Ok(NodeBundle {
        cert_pem: std::fs::read_to_string(plugin_cert_path(pki_dir, plugin_id)).with_context(
            || {
                format!(
                    "plugin cert not found for '{plugin_id}' — run `orca pki issue {plugin_id}`"
                )
            },
        )?,
        key_pem: std::fs::read_to_string(plugin_key_path(pki_dir, plugin_id))
            .with_context(|| format!("plugin key not found for '{plugin_id}'"))?,
        ca_cert_pem: std::fs::read_to_string(ca_cert_path(pki_dir))
            .context("CA cert not found — run `orca pki ca-init`")?,
    })
}

// ── rustls helpers ────────────────────────────────────────────────────────────

/// Build a rustls `CertificateDer` + `PrivateKeyDer` from PEM strings.
pub fn parse_cert_and_key(
    cert_pem: &str,
    key_pem: &str,
) -> Result<(
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
)> {
    use rustls_pemfile::{certs, private_key};

    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        certs(&mut cert_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("parsing cert chain")?;

    let key = private_key(&mut key_pem.as_bytes())
        .context("parsing private key")?
        .context("no private key found in PEM")?;

    Ok((cert_chain, key))
}

/// Build a `RootCertStore` containing the CA cert.
pub fn ca_root_store(ca_cert_pem: &str) -> Result<rustls::RootCertStore> {
    use rustls_pemfile::certs;

    let mut store = rustls::RootCertStore::empty();
    for der in certs(&mut ca_cert_pem.as_bytes()) {
        store.add(der.context("parsing CA cert")?)?;
    }
    Ok(store)
}

// ── List ──────────────────────────────────────────────────────────────────────

/// Names of all issued plugin certs in the PKI directory.
pub fn list_plugins(pki_dir: &Path) -> Vec<String> {
    let plugins_dir = pki_dir.join("plugins");
    std::fs::read_dir(&plugins_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

// ── Peer cert introspection ───────────────────────────────────────────────────

/// Extract the Subject Common Name from a DER-encoded leaf cert.
///
/// The plugin host calls this on the peer's leaf cert during the mTLS
/// handshake, then binds the resulting CN to the connection. Plugins are
/// then forced to identify as their cert's CN in `orca/hello`, closing the
/// trust gap where any cert signed by the orca CA could claim any plugin id.
pub fn peer_common_name(cert_der: &[u8]) -> Result<String> {
    let (_, parsed) =
        x509_parser::parse_x509_certificate(cert_der).context("parse peer cert DER")?;
    let cn = parsed
        .subject()
        .iter_common_name()
        .next()
        .context("peer cert has no Subject CN")?;
    let cn = cn
        .as_str()
        .context("peer cert CN is not valid UTF-8")?
        .to_string();
    Ok(cn)
}

// ── Bootstrap signing key (pod pre-join channel) ─────────────────────────────
//
// Every orca generates a per-host Ed25519 keypair on first boot, persisted as
// PKCS8 PEM under `<pki_dir>/bootstrap.{key,pub}.pem`. It is INDEPENDENT of
// the mesh CA — it exists precisely so a brand-new orca with no CA can still
// have a cryptographic identity over the pod/offer + pod/join-confirm wire.
//
// The same key also backs the self-signed TLS cert presented on the
// `pod-bootstrap.orca.local` SNI, so a joiner's verification reduces to:
// "mDNS-advertised fingerprint == bootstrap-TLS cert fingerprint == frame
// signer pubkey." One identity, one fingerprint to verify, no separate trust
// anchors.

pub fn bootstrap_key_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("bootstrap.key.pem")
}
pub fn bootstrap_pub_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("bootstrap.pub.pem")
}

/// Load the bootstrap signing key, generating it on first call. Idempotent.
pub fn load_or_init_bootstrap_key(pki_dir: &Path) -> Result<ed25519_dalek::SigningKey> {
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};

    let key_path = bootstrap_key_path(pki_dir);
    if key_path.exists() {
        let pem = std::fs::read_to_string(&key_path).context("read bootstrap key")?;
        let signing = SigningKey::from_pkcs8_pem(&pem).context("parse bootstrap key PEM")?;
        return Ok(signing);
    }

    std::fs::create_dir_all(pki_dir)
        .with_context(|| format!("create pki dir {}", pki_dir.display()))?;

    let mut seed = [0u8; 32];
    use rand::Rng;
    rand::rng().fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);

    let key_pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow::anyhow!("encode bootstrap key: {e}"))?
        .to_string();
    write_pem(key_path, &key_pem)?;

    let pub_pem = signing
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| anyhow::anyhow!("encode bootstrap pub: {e}"))?;
    write_pem(bootstrap_pub_path(pki_dir), &pub_pem)?;

    Ok(signing)
}

/// First 16 bytes of `SHA-256(verifying_key)` hex-encoded (32 chars). This is
/// what mDNS TXT advertises and what `pod pending` shows the user for visual
/// verification — short enough to read, long enough to be collision-resistant
/// within a pod.
pub fn bootstrap_pubkey_fingerprint(verifying: &ed25519_dalek::VerifyingKey) -> String {
    let mut h = Sha256::new();
    h.update(verifying.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(32);
    for b in &d[..16] {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

// ── Bootstrap TLS cert (self-signed, backed by the bootstrap key) ────────────

pub const POD_BOOTSTRAP_SAN: &str = "pod-bootstrap.orca.local";

pub fn bootstrap_cert_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("bootstrap.cert.pem")
}

/// Generate (or load) a self-signed TLS cert whose subject pubkey IS the
/// host's bootstrap Ed25519 pubkey. The cert SAN is `pod-bootstrap.orca.local`
/// so the plugin host can route this SNI to the pre-join handler.
///
/// Because the cert key == the bootstrap key, verifying the cert at TLS
/// handshake is equivalent to authenticating the bootstrap identity — no
/// separate signed-frame envelope is needed on the wire.
pub fn load_or_init_bootstrap_cert(pki_dir: &Path) -> Result<(String, String)> {
    let cert_path = bootstrap_cert_path(pki_dir);
    let key_path = bootstrap_key_path(pki_dir);

    // Ensure the key exists; the cert without the key would be useless.
    load_or_init_bootstrap_key(pki_dir)?;
    let key_pem = std::fs::read_to_string(&key_path).context("read bootstrap key")?;

    if cert_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path).context("read bootstrap cert")?;
        return Ok((cert_pem, key_pem));
    }

    let kp = KeyPair::from_pem(&key_pem).context("load bootstrap key as rcgen KeyPair")?;
    let mut params = CertificateParams::new(vec![POD_BOOTSTRAP_SAN.to_string()])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    set_validity_days(&mut params, BOOTSTRAP_CERT_VALIDITY_DAYS);
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-pod-bootstrap");
        dn.push(DnType::OrganizationName, "orca");
        params.distinguished_name = dn;
    }
    let cert = params
        .self_signed(&kp)
        .context("self-sign bootstrap cert")?;
    let cert_pem = cert.pem();
    write_pem(cert_path, &cert_pem)?;
    Ok((cert_pem, key_pem))
}

/// rustls client-side verifier that pins a single expected bootstrap pubkey
/// fingerprint (the first-16-byte SHA-256 hex of the SPKI). Use this when
/// dialing `pod-bootstrap.orca.local` with a pubkey known out-of-band (mDNS TXT
/// or an explicit --fingerprint flag).
pub fn pinned_bootstrap_verifier(
    expected_fp: String,
) -> std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier> {
    std::sync::Arc::new(PinnedFpVerifier { expected_fp })
}

/// TOFU bootstrap verifier — accepts the FIRST server cert it sees, stores
/// its SPKI fingerprint into `captured`, and only fails when the cert can't
/// be parsed. Used by joiner-initiated `pod join` / `pod connect` where the
/// joiner doesn't yet know the inviter's fp; the captured fp is later
/// cross-checked against the signed `RequestOfferResult.inviter_pubkey_fp`
/// echoed back in the JSON-RPC response.
///
/// **Not a replacement for pinning.** Callers MUST verify the captured fp
/// matches the signed echo before persisting state.
pub fn capturing_bootstrap_verifier(
    captured: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) -> std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier> {
    std::sync::Arc::new(CapturingFpVerifier { captured })
}

#[derive(Debug)]
struct CapturingFpVerifier {
    captured: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl rustls::client::danger::ServerCertVerifier for CapturingFpVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = spki_fingerprint_der(end_entity.as_ref())
            .map_err(|e| rustls::Error::General(format!("extract SPKI fp from cert: {e}")))?;
        if let Ok(mut slot) = self.captured.lock() {
            *slot = Some(fp);
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOfferedOrEnabled,
        ))
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
}

#[derive(Debug)]
struct PinnedFpVerifier {
    expected_fp: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedFpVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = spki_fingerprint_der(end_entity.as_ref())
            .map_err(|e| rustls::Error::General(format!("extract SPKI fp from cert: {e}")))?;
        if actual == self.expected_fp {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "pinned bootstrap pubkey mismatch: expected {} got {}",
                self.expected_fp, actual
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // Bootstrap channel is TLS 1.3 only; this should never be called.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOfferedOrEnabled,
        ))
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // We've pinned the pubkey directly; rustls still calls into here to
        // verify the handshake signature itself. Delegate to the default ring
        // crypto provider's algorithms.
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        // Ed25519 only — matches what the server cert uses.
        vec![rustls::SignatureScheme::ED25519]
    }
}

/// Compute the bootstrap-style fingerprint from a cert DER (i.e. first 16
/// bytes of `SHA-256(SubjectPublicKeyInfo)` hex). Matches the fp format
/// that mDNS TXT carries and that `bootstrap_pubkey_fingerprint` returns.
pub fn spki_fingerprint_der(cert_der: &[u8]) -> Result<String> {
    let (_, parsed) =
        x509_parser::parse_x509_certificate(cert_der).context("parse cert DER for SPKI")?;
    // SubjectPublicKeyInfo's raw_public_key is the actual key bytes
    // (Ed25519: 32 bytes). Hashing this rather than the whole SPKI ensures
    // the fp == bootstrap_pubkey_fingerprint(verifying_key).
    let pk_bytes = parsed.public_key().subject_public_key.data.as_ref();
    let mut h = Sha256::new();
    h.update(pk_bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(32);
    for b in &d[..16] {
        write!(s, "{b:02x}").unwrap();
    }
    Ok(s)
}

// ── Signed envelope (application-layer authentication for bootstrap wire) ────
//
// The bootstrap SNI lets a no-cert client connect, so the server has no TLS
// identity for the caller. We wrap the JSON-RPC body in a signed envelope so
// the receiver can prove who sent it: an Ed25519 signature over the
// canonical-JSON payload + the signer's pubkey for fp lookup.
//
// Flow:
//   pod/offer    — inviter signs, joiner verifies signer fp against the
//                  inviter's mDNS-advertised pubkey (or any prior pinned fp).
//   pod/join-confirm — joiner signs, inviter verifies signer fp against the
//                  peer_pubkey_fp on the pending offer.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignedEnvelope {
    /// Canonical JSON of the payload body. We serialize once and sign that
    /// exact byte sequence so verification doesn't re-serialize and risk a
    /// byte-difference.
    pub payload: String,
    /// Ed25519 verifying key (32 raw bytes), base64 standard.
    pub signer_pubkey_b64: String,
    /// Ed25519 signature (64 raw bytes), base64 standard.
    pub signature_b64: String,
}

/// Sign `body` with the host's bootstrap key. Caller is expected to keep
/// `body` shape stable across versions — the canonical-JSON we use is
/// `serde_json`'s default emitter (no key sorting), so the same struct on
/// the same code version round-trips exactly.
pub fn sign_envelope<T: serde::Serialize>(
    signing: &ed25519_dalek::SigningKey,
    body: &T,
) -> Result<SignedEnvelope> {
    use ed25519_dalek::Signer;
    let payload = serde_json::to_string(body).context("serialize envelope payload")?;
    let sig = signing.sign(payload.as_bytes());
    Ok(SignedEnvelope {
        payload,
        signer_pubkey_b64: crate::encoding::base64_encode(signing.verifying_key().as_bytes()),
        signature_b64: crate::encoding::base64_encode(&sig.to_bytes()),
    })
}

/// Verify and decode a signed envelope. Returns the decoded body plus the
/// signer's verifying key (so the caller can compute its fp and check it
/// against an expected value).
pub fn verify_envelope<T: serde::de::DeserializeOwned>(
    env: &SignedEnvelope,
) -> Result<(T, ed25519_dalek::VerifyingKey)> {
    use ed25519_dalek::Verifier;

    let pk_bytes =
        crate::encoding::base64_decode(&env.signer_pubkey_b64).context("decode signer pubkey")?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signer pubkey must be 32 bytes (got {})", pk_bytes.len()))?;
    let verifying =
        ed25519_dalek::VerifyingKey::from_bytes(&pk_arr).context("parse signer pubkey")?;

    let sig_bytes =
        crate::encoding::base64_decode(&env.signature_b64).context("decode signature")?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature must be 64 bytes (got {})", sig_bytes.len()))?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);

    verifying
        .verify(env.payload.as_bytes(), &sig)
        .context("envelope signature did not verify")?;

    let body: T = serde_json::from_str(&env.payload).context("parse envelope payload")?;
    Ok((body, verifying))
}

// ── Cert fingerprint (display) ───────────────────────────────────────────────

/// Standard `openssl x509 -fingerprint -sha256` format: uppercase colon-hex
/// of `SHA-256(cert DER)`. Used for the bootstrap-TLS cert pinning check and
/// for human-visible identity strings.
pub fn cert_fingerprint(cert_pem: &str) -> Result<String> {
    use rustls_pemfile::certs;
    let mut reader = cert_pem.as_bytes();
    let der = certs(&mut reader)
        .next()
        .context("no certificate in PEM")?
        .context("parse cert DER")?;
    let mut h = Sha256::new();
    h.update(&der);
    let d = h.finalize();
    let mut s = String::with_capacity(95);
    for (i, b) in d.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        write!(s, "{b:02X}").unwrap();
    }
    Ok(s)
}

/// Structured snapshot of one PEM-encoded cert. CN comes from the subject's
/// commonName; falls back to the empty string when absent.
#[derive(Debug, Clone)]
pub struct CertSummary {
    pub cn: String,
    pub fingerprint: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub days_remaining: i64,
}

pub fn cert_summary(cert_pem: &str) -> Result<CertSummary> {
    use rustls_pemfile::certs;
    let mut reader = cert_pem.as_bytes();
    let der = certs(&mut reader)
        .next()
        .context("no certificate in PEM")?
        .context("parse cert DER")?;
    let (_, parsed) = x509_parser::parse_x509_certificate(der.as_ref()).context("parse cert")?;
    let cn = parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|c| c.as_str().ok())
        .unwrap_or("")
        .to_string();
    let issued_at = parsed.validity().not_before.timestamp();
    let expires_at = parsed.validity().not_after.timestamp();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let days_remaining = (expires_at - now) / 86_400;
    let fingerprint = cert_fingerprint(cert_pem)?;
    Ok(CertSummary {
        cn,
        fingerprint,
        issued_at,
        expires_at,
        days_remaining,
    })
}

// ── Address parsing ──────────────────────────────────────────────────────────

/// Parse `host[:port]` with sensible support for bare hostnames, IPv4
/// literals, and bracketed IPv6 (`[::1]:12002`). Returns `(host, port)`.
/// `default_port` is used when the input has no `:port` suffix.
///
/// Single chokepoint so the wire path, CLI args, and mDNS records all agree
/// on the grammar.
pub fn parse_peer_addr(s: &str, default_port: u16) -> Result<(String, u16)> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty peer address");

    if let Some(rest) = s.strip_prefix('[') {
        let close = rest
            .find(']')
            .context("malformed IPv6 literal — missing ']'")?;
        let host = &rest[..close];
        anyhow::ensure!(!host.is_empty(), "empty IPv6 host");
        let after = &rest[close + 1..];
        let port = if after.is_empty() {
            default_port
        } else if let Some(p) = after.strip_prefix(':') {
            p.parse::<u16>().context("invalid port")?
        } else {
            anyhow::bail!("unexpected characters after IPv6 literal: {after}");
        };
        return Ok((host.to_string(), port));
    }

    let colon_count = s.chars().filter(|c| *c == ':').count();
    match colon_count {
        0 => Ok((s.to_string(), default_port)),
        1 => {
            let (h, p) = s.rsplit_once(':').unwrap();
            anyhow::ensure!(!h.is_empty(), "empty host");
            Ok((h.to_string(), p.parse::<u16>().context("invalid port")?))
        }
        // Bare IPv6 with no brackets and no port: e.g. `::1` or `fe80::1`.
        _ => Ok((s.to_string(), default_port)),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn write_pem(path: PathBuf, pem: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, pem.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    // Restrict key files to owner-only read/write.
    #[cfg(unix)]
    if path.to_string_lossy().contains(".key.") {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_issue_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();

        init(pki).unwrap();
        assert!(ca_cert_path(pki).exists());
        assert!(ca_key_path(pki).exists());
        assert!(server_cert_path(pki).exists());
        assert!(server_key_path(pki).exists());

        let bundle = issue(pki, "my-plugin", Capability::General).unwrap();
        assert!(!bundle.cert_pem.is_empty());
        assert!(!bundle.key_pem.is_empty());
        assert!(!bundle.ca_cert_pem.is_empty());

        assert!(plugin_cert_path(pki, "my-plugin").exists());
        assert!(plugin_key_path(pki, "my-plugin").exists());
    }

    #[test]
    fn init_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init(pki).unwrap();
        let ca_content = std::fs::read(ca_cert_path(pki)).unwrap();
        init(pki).unwrap(); // second call is a no-op
        assert_eq!(ca_content, std::fs::read(ca_cert_path(pki)).unwrap());
    }

    #[test]
    fn issue_fails_without_ca() {
        let dir = tempfile::tempdir().unwrap();
        let result = issue(dir.path(), "my-plugin", Capability::General);
        assert!(result.is_err());
    }

    #[test]
    fn list_plugins_returns_ids() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init(pki).unwrap();
        issue(pki, "plugin-a", Capability::General).unwrap();
        issue(pki, "plugin-b", Capability::Sensitive).unwrap();
        let mut ids = list_plugins(pki);
        ids.sort();
        assert_eq!(ids, vec!["plugin-a", "plugin-b"]);
    }

    #[test]
    fn parse_cert_and_key_works() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init(pki).unwrap();
        let bundle = load_server(pki).unwrap();
        let (chain, _key) = parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem).unwrap();
        assert!(!chain.is_empty());
    }

    #[test]
    fn bootstrap_key_generates_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        let k1 = load_or_init_bootstrap_key(pki).unwrap();
        assert!(bootstrap_key_path(pki).exists());
        assert!(bootstrap_pub_path(pki).exists());
        let k2 = load_or_init_bootstrap_key(pki).unwrap();
        assert_eq!(
            k1.to_bytes(),
            k2.to_bytes(),
            "second load must return same key"
        );
    }

    #[test]
    fn bootstrap_fingerprint_is_stable_and_hex() {
        let dir = tempfile::tempdir().unwrap();
        let k = load_or_init_bootstrap_key(dir.path()).unwrap();
        let fp = bootstrap_pubkey_fingerprint(&k.verifying_key());
        assert_eq!(fp.len(), 32, "fingerprint is first 16 bytes hex = 32 chars");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn bootstrap_sign_verify_roundtrip() {
        use ed25519_dalek::{Signer, Verifier};
        let dir = tempfile::tempdir().unwrap();
        let k = load_or_init_bootstrap_key(dir.path()).unwrap();
        let msg = b"orca bootstrap envelope";
        let sig = k.sign(msg);
        k.verifying_key().verify(msg, &sig).unwrap();
    }

    #[test]
    fn cert_fingerprint_format() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init(pki).unwrap();
        let bundle = load_server(pki).unwrap();
        let fp = cert_fingerprint(&bundle.cert_pem).unwrap();
        // SHA-256 colon-hex: 32 bytes → 64 hex chars + 31 colons = 95
        assert_eq!(fp.len(), 95);
        assert!(fp.contains(':'));
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() || c == ':'));
    }

    #[test]
    fn parse_peer_addr_variants() {
        // bare host
        assert_eq!(
            parse_peer_addr("host-g", 12002).unwrap(),
            ("host-g".into(), 12002)
        );
        // host:port
        assert_eq!(
            parse_peer_addr("host-g:9999", 12002).unwrap(),
            ("host-g".into(), 9999)
        );
        // IPv4
        assert_eq!(
            parse_peer_addr("10.0.0.5", 12002).unwrap(),
            ("10.0.0.5".into(), 12002)
        );
        // IPv4:port
        assert_eq!(
            parse_peer_addr("10.0.0.5:443", 12002).unwrap(),
            ("10.0.0.5".into(), 443)
        );
        // bracketed IPv6
        assert_eq!(
            parse_peer_addr("[::1]:8080", 12002).unwrap(),
            ("::1".into(), 8080)
        );
        // bracketed IPv6 without port
        assert_eq!(
            parse_peer_addr("[fe80::1]", 12002).unwrap(),
            ("fe80::1".into(), 12002)
        );
        // bare IPv6 → default port
        assert_eq!(
            parse_peer_addr("fe80::1", 12002).unwrap(),
            ("fe80::1".into(), 12002)
        );
        // whitespace tolerated
        assert_eq!(
            parse_peer_addr("  host-g:1  ", 12002).unwrap(),
            ("host-g".into(), 1)
        );
        // errors
        assert!(parse_peer_addr("", 12002).is_err());
        assert!(parse_peer_addr(":12345", 12002).is_err());
        assert!(parse_peer_addr("host-g:notaport", 12002).is_err());
    }

    #[test]
    fn bootstrap_cert_pubkey_matches_bootstrap_key_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        let signing = load_or_init_bootstrap_key(pki).unwrap();
        let expected_fp = bootstrap_pubkey_fingerprint(&signing.verifying_key());

        let (cert_pem, _key_pem) = load_or_init_bootstrap_cert(pki).unwrap();
        assert!(bootstrap_cert_path(pki).exists());

        let (chain, _) = parse_cert_and_key(&cert_pem, &_key_pem).unwrap();
        let fp_from_cert = spki_fingerprint_der(&chain[0]).unwrap();
        assert_eq!(
            fp_from_cert, expected_fp,
            "bootstrap cert SPKI fp must equal bootstrap key fp"
        );
    }

    #[test]
    fn bootstrap_cert_reload_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        let (c1, k1) = load_or_init_bootstrap_cert(pki).unwrap();
        let (c2, k2) = load_or_init_bootstrap_cert(pki).unwrap();
        assert_eq!(c1, c2);
        assert_eq!(k1, k2);
    }

    #[test]
    fn signed_envelope_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let k = load_or_init_bootstrap_key(dir.path()).unwrap();
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Body {
            code_hash: String,
            ttl: u32,
        }
        let body = Body {
            code_hash: "abc".into(),
            ttl: 300,
        };
        let env = sign_envelope(&k, &body).unwrap();
        let (decoded, vk): (Body, _) = verify_envelope(&env).unwrap();
        assert_eq!(decoded, body);
        assert_eq!(vk.as_bytes(), k.verifying_key().as_bytes());
    }

    #[test]
    fn signed_envelope_rejects_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let k = load_or_init_bootstrap_key(dir.path()).unwrap();
        let mut env = sign_envelope(&k, &"hello").unwrap();
        env.payload.push('x');
        let r: Result<(String, _)> = verify_envelope(&env);
        assert!(r.is_err());
    }

    #[test]
    fn peer_cert_validity_is_30d_window() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        let server_pem = std::fs::read_to_string(mesh_server_cert_path(pki)).unwrap();
        let days = cert_days_remaining(&server_pem).unwrap();
        // Just-issued; allow some slack for clock granularity but it should
        // be near PEER_VALIDITY_DAYS, not anywhere close to 365 or 2*365.
        assert!(
            days <= PEER_VALIDITY_DAYS,
            "got {days}d, expected <= {}",
            PEER_VALIDITY_DAYS
        );
        assert!(
            days >= PEER_VALIDITY_DAYS - 2,
            "got {days}d, expected >= {}",
            PEER_VALIDITY_DAYS - 2
        );
    }

    #[test]
    fn ca_cert_validity_is_one_year() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        let ca_pem = std::fs::read_to_string(mesh_ca_cert_path(pki)).unwrap();
        let days = cert_days_remaining(&ca_pem).unwrap();
        assert!((363..=365).contains(&days), "got {days}d");
    }

    #[test]
    fn should_rotate_fires_inside_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        let server_pem = std::fs::read_to_string(mesh_server_cert_path(pki)).unwrap();
        // Just-issued 30d cert; threshold 7d → should NOT rotate.
        assert!(!should_rotate(&server_pem, 7).unwrap());
        // Threshold 60d (bigger than the cert's lifetime) → SHOULD rotate.
        assert!(should_rotate(&server_pem, 60).unwrap());
    }

    #[test]
    fn reissue_swaps_cert_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        let before = std::fs::read_to_string(mesh_server_cert_path(pki)).unwrap();
        // Wait long enough that not_before differs (clock resolution).
        std::thread::sleep(std::time::Duration::from_secs(1));
        reissue_mesh_server_cert(pki).unwrap();
        let after = std::fs::read_to_string(mesh_server_cert_path(pki)).unwrap();
        assert_ne!(before, after, "reissue must produce a different cert");
        // New cert still validates (chain intact).
        let (chain, _) = parse_cert_and_key(
            &after,
            &std::fs::read_to_string(mesh_server_key_path(pki)).unwrap(),
        )
        .unwrap();
        assert!(!chain.is_empty());
    }

    #[test]
    fn atomic_write_replaces_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("foo.cert.pem");
        atomic_write_pem(&p, "v1").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "v1");
        atomic_write_pem(&p, "v2").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "v2");
        // tmp file is cleaned up.
        assert!(!p.with_extension("pem.tmp").exists());
    }

    #[test]
    fn ca_rotate_moves_current_to_previous() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        let before_cur_cert = std::fs::read_to_string(mesh_ca_cert_path(pki)).unwrap();
        let before_cur_key = std::fs::read_to_string(mesh_ca_key_path(pki)).unwrap();

        rotate_mesh_ca(pki).unwrap();
        assert!(has_mesh_ca_previous(pki));
        let prev_cert = std::fs::read_to_string(mesh_ca_previous_cert_path(pki)).unwrap();
        let prev_key = std::fs::read_to_string(mesh_ca_previous_key_path(pki)).unwrap();
        assert_eq!(prev_cert, before_cur_cert);
        assert_eq!(prev_key, before_cur_key);

        let new_cur = std::fs::read_to_string(mesh_ca_cert_path(pki)).unwrap();
        assert_ne!(new_cur, before_cur_cert, "current CA must be fresh");
    }

    #[test]
    fn drop_previous_is_idempotent_and_clears_slot() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        rotate_mesh_ca(pki).unwrap();
        assert!(has_mesh_ca_previous(pki));
        drop_mesh_ca_previous(pki).unwrap();
        assert!(!has_mesh_ca_previous(pki));
        // Second call: no-op, no error.
        drop_mesh_ca_previous(pki).unwrap();
    }

    #[test]
    fn root_store_spans_two_cas() {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init_mesh_ca(pki, "host-g").unwrap();
        let old_ca = std::fs::read_to_string(mesh_ca_cert_path(pki)).unwrap();
        rotate_mesh_ca(pki).unwrap();
        let new_ca = std::fs::read_to_string(mesh_ca_cert_path(pki)).unwrap();
        let store = ca_root_store_multi([new_ca.as_str(), old_ca.as_str()]).unwrap();
        assert_eq!(store.len(), 2, "trust store should hold both CAs");
    }

    #[test]
    fn import_state_round_trips_both_slots() {
        let src_dir = tempfile::tempdir().unwrap();
        init_mesh_ca(src_dir.path(), "host-g").unwrap();
        rotate_mesh_ca(src_dir.path()).unwrap();
        let cur_c = std::fs::read_to_string(mesh_ca_cert_path(src_dir.path())).unwrap();
        let cur_k = std::fs::read_to_string(mesh_ca_key_path(src_dir.path())).unwrap();
        let prv_c = std::fs::read_to_string(mesh_ca_previous_cert_path(src_dir.path())).unwrap();
        let prv_k = std::fs::read_to_string(mesh_ca_previous_key_path(src_dir.path())).unwrap();

        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(mesh_dir(dst.path())).unwrap();
        import_mesh_ca_state(dst.path(), &cur_c, &cur_k, Some(&prv_c), Some(&prv_k)).unwrap();
        assert!(has_mesh_ca_key(dst.path()));
        assert!(has_mesh_ca_previous(dst.path()));
    }

    #[test]
    fn capability_round_trip_and_display() {
        assert_eq!(Capability::General.as_str(), "general");
        assert_eq!(Capability::Sensitive.as_str(), "sensitive");
        assert_eq!(format!("{}", Capability::General), "general");
        assert_eq!(format!("{}", Capability::Sensitive), "sensitive");
        assert!(matches!(
            "general".parse::<Capability>().unwrap(),
            Capability::General
        ));
        assert!(matches!(
            "sensitive".parse::<Capability>().unwrap(),
            Capability::Sensitive
        ));
        assert!("bogus".parse::<Capability>().is_err());
    }

    #[test]
    fn load_mesh_bundles_errors_when_uninitialized() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_mesh_server(dir.path()).is_err());
        assert!(load_mesh_client(dir.path()).is_err());
        assert!(!has_mesh_ca_key(dir.path()));
    }

    #[test]
    fn load_mesh_bundles_succeed_after_init() {
        let dir = tempfile::tempdir().unwrap();
        init_mesh_ca(dir.path(), "host-load").unwrap();
        let s = load_mesh_server(dir.path()).unwrap();
        assert!(s.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(s.key_pem.contains("BEGIN"));
        let c = load_mesh_client(dir.path()).unwrap();
        assert!(c.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(has_mesh_ca_key(dir.path()));
    }

    #[test]
    fn build_peer_csr_both_roles_and_sign_full_flow() {
        let dir = tempfile::tempdir().unwrap();
        init_mesh_ca(dir.path(), "founder").unwrap();
        let (csr_c, key_c) = build_peer_csr("joiner", PeerRole::Client).unwrap();
        assert!(csr_c.contains("CERTIFICATE REQUEST"));
        assert!(key_c.contains("BEGIN"));
        let (csr_s, _) = build_peer_csr("joiner", PeerRole::Server).unwrap();
        assert!(csr_s.contains("CERTIFICATE REQUEST"));

        let (cert_c, ca_c) = sign_peer_csr(dir.path(), &csr_c, "joiner", PeerRole::Client).unwrap();
        assert!(cert_c.contains("BEGIN CERTIFICATE"));
        assert!(ca_c.contains("BEGIN CERTIFICATE"));
        let (cert_s, _) = sign_peer_csr(dir.path(), &csr_s, "joiner", PeerRole::Server).unwrap();
        assert!(cert_s.contains("BEGIN CERTIFICATE"));

        // Verify CN was rewritten per role regardless of CSR contents.
        let summary_c = cert_summary(&cert_c).unwrap();
        assert_eq!(summary_c.cn, "joiner");
        let summary_s = cert_summary(&cert_s).unwrap();
        assert_eq!(summary_s.cn, "orca-pod-server");
    }

    #[test]
    fn sign_peer_csr_rejects_without_ca_key() {
        let dir = tempfile::tempdir().unwrap();
        let (csr, _) = build_peer_csr("x", PeerRole::Client).unwrap();
        let err = sign_peer_csr(dir.path(), &csr, "x", PeerRole::Client)
            .err()
            .unwrap();
        assert!(format!("{err}").contains("mesh CA private key"));
    }

    #[test]
    fn export_and_import_mesh_ca_keypair_round_trip() {
        let src = tempfile::tempdir().unwrap();
        init_mesh_ca(src.path(), "founder").unwrap();
        let (cert_pem, key_pem) = export_mesh_ca_keypair(src.path()).unwrap();

        // Set up a destination that already has the SAME CA cert (typical
        // post-join state) but no key.
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(mesh_dir(dst.path())).unwrap();
        std::fs::write(mesh_ca_cert_path(dst.path()), &cert_pem).unwrap();
        assert!(!has_mesh_ca_key(dst.path()));
        import_mesh_ca_keypair(dst.path(), &cert_pem, &key_pem).unwrap();
        assert!(has_mesh_ca_key(dst.path()));
    }

    #[test]
    fn import_mesh_ca_rejects_mismatched_cert() {
        let src = tempfile::tempdir().unwrap();
        init_mesh_ca(src.path(), "founder").unwrap();
        let (_, real_key) = export_mesh_ca_keypair(src.path()).unwrap();

        let other = tempfile::tempdir().unwrap();
        init_mesh_ca(other.path(), "founder").unwrap();
        let (other_cert, _) = export_mesh_ca_keypair(other.path()).unwrap();

        // dst has src's cert but we hand it other's cert during import
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(mesh_dir(dst.path())).unwrap();
        let (src_cert, _) = export_mesh_ca_keypair(src.path()).unwrap();
        std::fs::write(mesh_ca_cert_path(dst.path()), &src_cert).unwrap();

        let err = import_mesh_ca_keypair(dst.path(), &other_cert, &real_key)
            .err()
            .unwrap();
        assert!(format!("{err}").contains("does not match"));
    }

    #[test]
    fn export_mesh_ca_keypair_errors_when_no_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(export_mesh_ca_keypair(dir.path()).is_err());
    }

    #[test]
    fn reissue_mesh_client_cert_swaps_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        init_mesh_ca(dir.path(), "host-cli").unwrap();
        let before = std::fs::read_to_string(mesh_client_cert_path(dir.path())).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        reissue_mesh_client_cert(dir.path(), "host-cli").unwrap();
        let after = std::fs::read_to_string(mesh_client_cert_path(dir.path())).unwrap();
        assert_ne!(before, after);
        assert_eq!(cert_summary(&after).unwrap().cn, "host-cli");
    }

    #[test]
    fn reissue_client_errors_without_ca_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(reissue_mesh_client_cert(dir.path(), "x").is_err());
    }

    #[test]
    fn build_refresh_csrs_then_install_refreshed_peer_certs() {
        let dir = tempfile::tempdir().unwrap();
        init_mesh_ca(dir.path(), "host-r").unwrap();
        let (csr_c, key_c, csr_s, key_s) = build_refresh_csrs("host-r").unwrap();
        let (cert_c, _) = sign_peer_csr(dir.path(), &csr_c, "host-r", PeerRole::Client).unwrap();
        let (cert_s, _) = sign_peer_csr(dir.path(), &csr_s, "host-r", PeerRole::Server).unwrap();
        install_refreshed_peer_certs(dir.path(), &cert_c, &key_c, &cert_s, &key_s).unwrap();
        assert_eq!(
            std::fs::read_to_string(mesh_client_cert_path(dir.path())).unwrap(),
            cert_c
        );
        assert_eq!(
            std::fs::read_to_string(mesh_server_cert_path(dir.path())).unwrap(),
            cert_s
        );
    }

    #[test]
    fn peer_common_name_extracts_cn_from_signed_cert() {
        let dir = tempfile::tempdir().unwrap();
        init_mesh_ca(dir.path(), "founder").unwrap();
        let (csr, _) = build_peer_csr("alice", PeerRole::Client).unwrap();
        let (cert_pem, _) = sign_peer_csr(dir.path(), &csr, "alice", PeerRole::Client).unwrap();
        let (chain, _) = parse_cert_and_key(
            &cert_pem,
            &std::fs::read_to_string(mesh_client_key_path(dir.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(peer_common_name(&chain[0]).unwrap(), "alice");
    }

    #[test]
    fn peer_common_name_errors_on_garbage() {
        assert!(peer_common_name(&[0u8; 4]).is_err());
    }

    #[test]
    fn cert_summary_returns_populated_fields() {
        let dir = tempfile::tempdir().unwrap();
        init_mesh_ca(dir.path(), "host-cs").unwrap();
        let pem = std::fs::read_to_string(mesh_server_cert_path(dir.path())).unwrap();
        let s = cert_summary(&pem).unwrap();
        assert_eq!(s.cn, "orca-pod-server");
        assert!(!s.fingerprint.is_empty());
        assert!(s.expires_at > s.issued_at);
        assert!(s.days_remaining > 0);
    }

    #[test]
    fn cert_summary_errors_on_empty_pem() {
        assert!(cert_summary("").is_err());
    }

    #[test]
    fn rest_server_cert_localhost_san_and_browser_compat_true() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let pem = std::fs::read_to_string(server_cert_path(dir.path())).unwrap();
        assert!(rest_server_cert_has_localhost_san(&pem));
        assert!(rest_server_cert_is_browser_compatible(&pem));
    }

    #[test]
    fn rest_server_cert_predicates_false_on_garbage() {
        assert!(!rest_server_cert_has_localhost_san("not a pem"));
        assert!(!rest_server_cert_is_browser_compatible("not a pem"));
    }

    #[test]
    fn refresh_rest_server_cert_swaps_atomically() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let before = std::fs::read_to_string(server_cert_path(dir.path())).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        refresh_rest_server_cert(dir.path()).unwrap();
        let after = std::fs::read_to_string(server_cert_path(dir.path())).unwrap();
        assert_ne!(before, after);
        assert!(rest_server_cert_has_localhost_san(&after));
    }

    #[test]
    fn refresh_rest_server_cert_errors_without_ca() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refresh_rest_server_cert(dir.path()).is_err());
    }

    #[test]
    fn issue_cli_client_cert_creates_and_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let b1 = issue_cli_client_cert(dir.path(), "myhost").unwrap();
        assert!(b1.cert_pem.contains("BEGIN CERTIFICATE"));
        assert_eq!(cert_summary(&b1.cert_pem).unwrap().cn, "cli.myhost");
        let b2 = issue_cli_client_cert(dir.path(), "myhost").unwrap();
        assert_eq!(b1.cert_pem, b2.cert_pem, "second call must reuse on-disk");
    }

    #[test]
    fn issue_cli_client_cert_errors_without_ca() {
        let dir = tempfile::tempdir().unwrap();
        assert!(issue_cli_client_cert(dir.path(), "h").is_err());
    }

    #[test]
    fn load_cli_client_returns_none_until_issued() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_cli_client(dir.path()).is_none());
        init(dir.path()).unwrap();
        issue_cli_client_cert(dir.path(), "h").unwrap();
        assert!(load_cli_client(dir.path()).is_some());
    }

    #[test]
    fn pinned_bootstrap_verifier_accepts_matching_fp_and_rejects_other() {
        let _p = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, _) = load_or_init_bootstrap_cert(dir.path()).unwrap();
        let (chain, _) = parse_cert_and_key(
            &cert_pem,
            &std::fs::read_to_string(bootstrap_key_path(dir.path())).unwrap(),
        )
        .unwrap();
        let actual_fp = spki_fingerprint_der(chain[0].as_ref()).unwrap();

        let v = pinned_bootstrap_verifier(actual_fp.clone());
        let sn = rustls::pki_types::ServerName::try_from("pod-bootstrap.orca.local").unwrap();
        let now = rustls::pki_types::UnixTime::now();
        assert!(v.verify_server_cert(&chain[0], &[], &sn, &[], now).is_ok());

        let v2 = pinned_bootstrap_verifier("0".repeat(32));
        assert!(
            v2.verify_server_cert(&chain[0], &[], &sn, &[], now)
                .is_err()
        );

        assert!(
            v.supported_verify_schemes()
                .contains(&rustls::SignatureScheme::ED25519)
        );
    }

    #[test]
    fn capturing_bootstrap_verifier_stores_fp() {
        let _p = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, _) = load_or_init_bootstrap_cert(dir.path()).unwrap();
        let (chain, _) = parse_cert_and_key(
            &cert_pem,
            &std::fs::read_to_string(bootstrap_key_path(dir.path())).unwrap(),
        )
        .unwrap();
        let expected = spki_fingerprint_der(chain[0].as_ref()).unwrap();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let v = capturing_bootstrap_verifier(slot.clone());
        let sn = rustls::pki_types::ServerName::try_from("pod-bootstrap.orca.local").unwrap();
        let now = rustls::pki_types::UnixTime::now();
        v.verify_server_cert(&chain[0], &[], &sn, &[], now).unwrap();
        assert_eq!(slot.lock().unwrap().as_deref(), Some(expected.as_str()));
        assert!(
            v.supported_verify_schemes()
                .contains(&rustls::SignatureScheme::ED25519)
        );
    }

    #[test]
    fn ca_paths_and_plugin_paths_are_under_pki_dir() {
        let p = std::path::Path::new("/tmp/pkitest");
        assert!(ca_cert_path(p).ends_with("ca.cert.pem"));
        assert!(ca_key_path(p).ends_with("ca.key.pem"));
        assert!(server_cert_path(p).ends_with("server/node.cert.pem"));
        assert!(server_key_path(p).ends_with("server/node.key.pem"));
        assert!(plugin_cert_path(p, "x").ends_with("plugins/x/node.cert.pem"));
        assert!(plugin_key_path(p, "x").ends_with("plugins/x/node.key.pem"));
        assert!(cli_client_cert_path(p).ends_with("client.cert.pem"));
        assert!(cli_client_key_path(p).ends_with("client.key.pem"));
        assert!(bootstrap_pub_path(p).ends_with("bootstrap.pub.pem"));
        assert!(bootstrap_cert_path(p).ends_with("bootstrap.cert.pem"));
    }

    #[test]
    fn rest_server_cert_uses_ecdsa_p256() {
        // The REST server cert (browser-facing) must be ECDSA P-256 —
        // browsers (Firefox/Chrome) reject Ed25519 leaf certs in TLS
        // server auth and surface "Secure Connection Failed" with no
        // Advanced bypass. ecPublicKey OID: 1.2.840.10045.2.1.
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path();
        init(pki).unwrap();
        let bundle = load_server(pki).unwrap();
        let (chain, _) = parse_cert_and_key(&bundle.cert_pem, &bundle.key_pem).unwrap();
        let (_, parsed) = x509_parser::parse_x509_certificate(&chain[0]).unwrap();
        let oid = parsed.subject_pki.algorithm.algorithm.to_id_string();
        assert_eq!(
            oid, "1.2.840.10045.2.1",
            "expected ecPublicKey OID, got {oid}"
        );
    }
}
