//! PKI: CA generation, node cert issuance, and cert/key loading.
//!
//! All material lives under `~/.orca/pki/`:
//!   ca.cert.pem / ca.key.pem          — root CA (generated once by `orca pki init`)
//!   server/node.cert.pem / node.key.pem — server cert (generated alongside the CA)
//!   plugins/<id>/node.cert.pem / node.key.pem — per-plugin cert
//!
//! Server cert DNS SAN: `core.orca.local`
//! Plugin cert DNS SAN: `<plugin-id>.plugin.orca.local`

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use std::path::{Path, PathBuf};

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

// ── File paths ────────────────────────────────────────────────────────────────

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
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-ca");
        dn.push(DnType::OrganizationName, "orca");
        ca_params.distinguished_name = dn;
    }
    let ca_cert = ca_params.self_signed(&ca_key)?;

    write_pem(ca_cert_path(pki_dir), &ca_cert.pem())?;
    write_pem(ca_key_path(pki_dir), &ca_key.serialize_pem())?;

    // Server cert
    let server_dir = pki_dir.join("server");
    std::fs::create_dir_all(&server_dir)?;

    let server_key = KeyPair::generate()?;
    let mut server_params = CertificateParams::new(vec!["core.orca.local".to_string()])?;
    server_params.is_ca = IsCa::NoCa;
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "orca-core");
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, "server");
        server_params.distinguished_name = dn;
    }
    let server_cert = server_params.signed_by(&server_key, &ca_cert, &ca_key)?;

    write_pem(server_cert_path(pki_dir), &server_cert.pem())?;
    write_pem(server_key_path(pki_dir), &server_key.serialize_pem())?;

    Ok(())
}

// ── Issue plugin cert ─────────────────────────────────────────────────────────

/// Issue a cert for `plugin_id` signed by the CA. Errors if the CA does not
/// exist — caller must run `init` first.
pub fn issue(pki_dir: &Path, plugin_id: &str, capability: Capability) -> Result<NodeBundle> {
    let ca_cert_pem = std::fs::read_to_string(ca_cert_path(pki_dir))
        .context("CA cert not found — run `orca pki init` first")?;
    let ca_key_pem = std::fs::read_to_string(ca_key_path(pki_dir))
        .context("CA key not found — run `orca pki init` first")?;

    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_cert = CertificateParams::from_ca_cert_pem(&ca_cert_pem)?.self_signed(&ca_key)?;

    let dns_san = format!("{plugin_id}.plugin.orca.local");
    let plugin_key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![dns_san])?;
    params.is_ca = IsCa::NoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, plugin_id);
        dn.push(DnType::OrganizationName, "orca");
        dn.push(DnType::OrganizationalUnitName, capability.as_str());
        params.distinguished_name = dn;
    }
    let plugin_cert = params.signed_by(&plugin_key, &ca_cert, &ca_key)?;

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

// ── Load ──────────────────────────────────────────────────────────────────────

/// Load the server's TLS material (cert chain + key + CA cert) from disk.
pub fn load_server(pki_dir: &Path) -> Result<NodeBundle> {
    Ok(NodeBundle {
        cert_pem: std::fs::read_to_string(server_cert_path(pki_dir))
            .context("server cert not found — run `orca pki init`")?,
        key_pem: std::fs::read_to_string(server_key_path(pki_dir))
            .context("server key not found — run `orca pki init`")?,
        ca_cert_pem: std::fs::read_to_string(ca_cert_path(pki_dir))
            .context("CA cert not found — run `orca pki init`")?,
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
            .context("CA cert not found — run `orca pki init`")?,
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
}
