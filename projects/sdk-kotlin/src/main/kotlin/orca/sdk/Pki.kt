// PKI: load PEM-encoded mTLS material for orca plugins.
//
// Layout under pkiDir mirrors projects/sdk/src/pki.rs:
//   ca.cert.pem
//   plugins/<id>/node.cert.pem, plugins/<id>/node.key.pem
//
// Plugins never generate CA / server / plugin certs — that is the host's job.
package orca.sdk

import java.io.ByteArrayInputStream
import java.nio.file.Files
import java.nio.file.Path
import java.security.KeyFactory
import java.security.KeyStore
import java.security.PrivateKey
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.security.spec.PKCS8EncodedKeySpec
import java.util.Base64
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.TrustManagerFactory

data class NodeBundle(
    val certPem: String,
    val keyPem: String,
    val caCertPem: String,
)

object Pki {
    fun caCertPath(pkiDir: Path): Path = pkiDir.resolve("ca.cert.pem")

    fun pluginCertPath(pkiDir: Path, pluginId: String): Path =
        pkiDir.resolve("plugins").resolve(pluginId).resolve("node.cert.pem")

    fun pluginKeyPath(pkiDir: Path, pluginId: String): Path =
        pkiDir.resolve("plugins").resolve(pluginId).resolve("node.key.pem")

    /** Load this plugin's cert + key + the signing CA cert from disk. */
    fun loadPlugin(pkiDir: Path, pluginId: String): NodeBundle = NodeBundle(
        certPem = Files.readString(pluginCertPath(pkiDir, pluginId)),
        keyPem = Files.readString(pluginKeyPath(pkiDir, pluginId)),
        caCertPem = Files.readString(caCertPath(pkiDir)),
    )

    /**
     * Build a JVM SSLContext that presents the bundle as the client
     * identity and verifies the server cert against bundle.caCertPem.
     */
    fun clientSslContext(bundle: NodeBundle): SSLContext {
        val cf = CertificateFactory.getInstance("X.509")
        val caCerts = cf.generateCertificates(ByteArrayInputStream(bundle.caCertPem.toByteArray()))
            .map { it as X509Certificate }
        val clientCerts = cf.generateCertificates(ByteArrayInputStream(bundle.certPem.toByteArray()))
            .map { it as X509Certificate }
            .toTypedArray()
        val privateKey = parsePrivateKey(bundle.keyPem)

        val trustStore = KeyStore.getInstance(KeyStore.getDefaultType()).apply {
            load(null, null)
            caCerts.forEachIndexed { i, c -> setCertificateEntry("ca-$i", c) }
        }
        val tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm())
            .apply { init(trustStore) }

        val keyStore = KeyStore.getInstance(KeyStore.getDefaultType()).apply {
            load(null, null)
            setKeyEntry("plugin", privateKey, charArrayOf(), clientCerts)
        }
        val kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm())
            .apply { init(keyStore, charArrayOf()) }

        return SSLContext.getInstance("TLSv1.3").apply {
            init(kmf.keyManagers, tmf.trustManagers, null)
        }
    }

    private fun parsePrivateKey(pem: String): PrivateKey {
        // PKCS#8 PEM body — strip headers, base64 decode, feed to JCA.
        val cleaned = pem.lineSequence()
            .filter { !it.startsWith("-----") }
            .joinToString("")
            .replace("\\s".toRegex(), "")
        val der = Base64.getDecoder().decode(cleaned)
        // Try common JVM-supported algorithms in order. rcgen on the host
        // emits PKCS#8 ECDSA P-256 by default; fall back to RSA / Ed25519.
        for (alg in listOf("EC", "RSA", "EdDSA", "Ed25519")) {
            try {
                return KeyFactory.getInstance(alg).generatePrivate(PKCS8EncodedKeySpec(der))
            } catch (_: Exception) {}
        }
        error("could not parse private key (tried EC, RSA, EdDSA, Ed25519)")
    }
}
