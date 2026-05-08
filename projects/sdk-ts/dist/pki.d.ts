import type { ConnectionOptions } from 'node:tls';
export interface NodeBundle {
    certPem: string;
    keyPem: string;
    caCertPem: string;
}
export declare const caCertPath: (pkiDir: string) => string;
export declare const pluginCertPath: (pkiDir: string, pluginID: string) => string;
export declare const pluginKeyPath: (pkiDir: string, pluginID: string) => string;
/** Load the plugin's cert + key + signing CA cert. */
export declare function loadPlugin(pkiDir: string, pluginID: string): Promise<NodeBundle>;
/**
 * Build Node tls.connect options that present the plugin bundle as the
 * client identity and verify the server cert against the CA cert.
 *
 * `servername` is "core.orca.local" — matches the SAN the host's server
 * cert is issued with.
 */
export declare function clientTlsOptions(bundle: NodeBundle): ConnectionOptions;
//# sourceMappingURL=pki.d.ts.map