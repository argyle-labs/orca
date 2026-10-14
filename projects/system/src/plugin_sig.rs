//! Minisign verification of plugin release binaries.
//!
//! Release CI signs every plugin asset with the trusted comment
//! `plugin=<name> version=<v> sha256=<hex>`. The signature covers that comment,
//! so a valid signature binds the binary's bytes to the plugin name it was
//! released as; a correctly signed binary of another plugin is refused.

use anyhow::{Context, Result, bail, ensure};
use minisign_verify::{PublicKey, Signature};

/// The argyle-labs plugin release key (key id AA954BEC52A1AED5), the same key
/// as `keys/plugin-release.pub` in the shared CI repo.
const ARGYLE_LABS_PLUGIN_KEY: &str = "RWTVrqFS7EuVqssBkmxckIHagYOZcLz4beDvFcAxtiDSqqVvl3fjobl4";

/// Base64 minisign public keys a plugin signature may verify against.
pub fn trusted_keys() -> &'static [&'static str] {
    &[ARGYLE_LABS_PLUGIN_KEY]
}

/// What a verified signature attests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPlugin {
    pub version: String,
    pub sha256: String,
}

/// Verify `minisig` over `bytes` against any of `keys`, then check its trusted
/// comment names `expected_name` and carries the sha256 of `bytes`.
pub fn verify(
    bytes: &[u8],
    minisig: &str,
    expected_name: &str,
    keys: &[&str],
) -> Result<SignedPlugin> {
    let sig = Signature::decode(minisig).context("malformed plugin signature")?;
    let mut errors = Vec::new();
    let verified = keys.iter().any(|k| {
        match PublicKey::from_base64(k).and_then(|pk| pk.verify(bytes, &sig, false)) {
            Ok(()) => true,
            Err(e) => {
                errors.push(e.to_string());
                false
            }
        }
    });
    ensure!(
        verified,
        "plugin '{expected_name}' signature does not verify against any trusted key ({})",
        errors.join("; ")
    );

    let comment = sig.trusted_comment();
    let field = |key: &str| {
        comment
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
            .with_context(|| format!("plugin signature comment {comment:?} has no `{key}=`"))
    };
    let name = field("plugin")?;
    let version = field("version")?;
    let sha256 = field("sha256")?.to_ascii_lowercase();
    if name != expected_name {
        bail!("signature is for plugin '{name}', not '{expected_name}'");
    }
    ensure!(
        sha256 == utils::hash::sha256_hex(bytes),
        "plugin '{expected_name}' signature attests sha256 {sha256}, which does not match the binary"
    );
    Ok(SignedPlugin {
        version: version.to_string(),
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &[u8] = include_bytes!("../tests/fixtures/plugin_sig/demo");
    const SIG: &str = include_str!("../tests/fixtures/plugin_sig/demo.minisig");

    fn key(file: &str) -> String {
        fixture(file).lines().nth(1).unwrap().to_string()
    }

    fn fixture(file: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/plugin_sig/{file}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn check(bytes: &[u8], sig: &str, name: &str, keyfile: &str) -> Result<SignedPlugin> {
        verify(bytes, sig, name, &[&key(keyfile)])
    }

    #[test]
    fn a_valid_signature_verifies() {
        let got = check(DEMO, SIG, "demo", "test.pub").unwrap();
        assert_eq!(got.version, "1.2.3");
        assert_eq!(got.sha256, utils::hash::sha256_hex(DEMO));
    }

    #[test]
    fn any_listed_key_may_verify() {
        let (other, test) = (key("other.pub"), key("test.pub"));
        verify(DEMO, SIG, "demo", &[&other, &test]).unwrap();
    }

    #[test]
    fn a_flipped_byte_fails() {
        let mut bytes = DEMO.to_vec();
        bytes[0] ^= 1;
        assert!(check(&bytes, SIG, "demo", "test.pub").is_err());
    }

    #[test]
    fn another_key_fails() {
        let err = check(DEMO, SIG, "demo", "other.pub").unwrap_err();
        assert!(err.to_string().contains("does not verify"), "{err:#}");
    }

    #[test]
    fn a_wrong_name_fails() {
        let err = check(DEMO, SIG, "other", "test.pub").unwrap_err();
        assert!(err.to_string().contains("not 'other'"), "{err:#}");
    }

    #[test]
    fn a_wrong_sha256_fails() {
        let sig = fixture("demo.wrong-sha.minisig");
        let err = check(DEMO, &sig, "demo", "test.pub").unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err:#}");
    }

    #[test]
    fn a_comment_without_the_fields_fails() {
        for f in ["demo.no-comment.minisig", "demo.no-sha.minisig"] {
            let err = check(DEMO, &fixture(f), "demo", "test.pub").unwrap_err();
            assert!(err.to_string().contains("has no"), "{f}: {err:#}");
        }
    }

    #[test]
    fn a_malformed_signature_fails() {
        assert!(check(DEMO, "not a signature", "demo", "test.pub").is_err());
        assert!(check(DEMO, "", "demo", "test.pub").is_err());
    }

    /// Proves the CI comment format against a real release asset. Set
    /// `ORCA_SIG_ASSET` to a downloaded binary with `<asset>.minisig` beside it
    /// and `ORCA_SIG_PLUGIN` to its plugin name.
    #[test]
    #[ignore]
    fn a_real_release_asset_verifies() {
        let asset = std::env::var("ORCA_SIG_ASSET").unwrap();
        let name = std::env::var("ORCA_SIG_PLUGIN").unwrap();
        let bytes = std::fs::read(&asset).unwrap();
        let sig = std::fs::read_to_string(format!("{asset}.minisig")).unwrap();
        verify(&bytes, &sig, &name, trusted_keys()).unwrap();
    }
}
