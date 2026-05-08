/**
 * PKI: load PEM-encoded mTLS material for orca plugins.
 *
 * Plugins never generate CA / server / plugin certs — that is the host's
 * responsibility (the Rust SDK's `pki::init` / `pki::issue`). TS plugins
 * only load their already-issued bundle from disk and present it during
 * the mTLS handshake.
 *
 * Layout under pkiDir mirrors projects/sdk/src/pki.rs:
 *   ca.cert.pem
 *   plugins/<id>/node.cert.pem, plugins/<id>/node.key.pem
 */
import { readFile } from 'node:fs/promises';
import path from 'node:path';
export const caCertPath = (pkiDir) => path.join(pkiDir, 'ca.cert.pem');
export const pluginCertPath = (pkiDir, pluginID) => path.join(pkiDir, 'plugins', pluginID, 'node.cert.pem');
export const pluginKeyPath = (pkiDir, pluginID) => path.join(pkiDir, 'plugins', pluginID, 'node.key.pem');
/** Load the plugin's cert + key + signing CA cert. */
export async function loadPlugin(pkiDir, pluginID) {
    const [certPem, keyPem, caCertPem] = await Promise.all([
        readFile(pluginCertPath(pkiDir, pluginID), 'utf8'),
        readFile(pluginKeyPath(pkiDir, pluginID), 'utf8'),
        readFile(caCertPath(pkiDir), 'utf8'),
    ]);
    return { certPem, keyPem, caCertPem };
}
/**
 * Build Node tls.connect options that present the plugin bundle as the
 * client identity and verify the server cert against the CA cert.
 *
 * `servername` is "core.orca.local" — matches the SAN the host's server
 * cert is issued with.
 */
export function clientTlsOptions(bundle) {
    return {
        cert: bundle.certPem,
        key: bundle.keyPem,
        ca: bundle.caCertPem,
        servername: 'core.orca.local',
        minVersion: 'TLSv1.3',
    };
}
//# sourceMappingURL=pki.js.map