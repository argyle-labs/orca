// Package pki loads PEM-encoded mTLS material for orca plugins.
//
// File layout under pki_dir (mirrors projects/sdk/src/pki.rs):
//
//	ca.cert.pem
//	server/node.cert.pem,  server/node.key.pem
//	plugins/<id>/node.cert.pem, plugins/<id>/node.key.pem
//
// Go ports never generate CA / server / plugin certs; that is the host's job
// (the Rust SDK's `pki::init` / `pki::issue`). Plugins only load their
// already-issued bundle and present it during the mTLS handshake.
package pki

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

// NodeBundle is the PEM-encoded cert + key + signing-CA bundle for a plugin.
type NodeBundle struct {
	CertPEM   []byte
	KeyPEM    []byte
	CACertPEM []byte
}

// CACertPath returns ca.cert.pem under pkiDir.
func CACertPath(pkiDir string) string { return filepath.Join(pkiDir, "ca.cert.pem") }

// PluginCertPath returns plugins/<id>/node.cert.pem under pkiDir.
func PluginCertPath(pkiDir, pluginID string) string {
	return filepath.Join(pkiDir, "plugins", pluginID, "node.cert.pem")
}

// PluginKeyPath returns plugins/<id>/node.key.pem under pkiDir.
func PluginKeyPath(pkiDir, pluginID string) string {
	return filepath.Join(pkiDir, "plugins", pluginID, "node.key.pem")
}

// LoadPlugin reads the plugin's cert + key + the signing CA cert.
func LoadPlugin(pkiDir, pluginID string) (*NodeBundle, error) {
	cert, err := os.ReadFile(PluginCertPath(pkiDir, pluginID))
	if err != nil {
		return nil, fmt.Errorf("read plugin cert for %q: %w", pluginID, err)
	}
	key, err := os.ReadFile(PluginKeyPath(pkiDir, pluginID))
	if err != nil {
		return nil, fmt.Errorf("read plugin key for %q: %w", pluginID, err)
	}
	caCert, err := os.ReadFile(CACertPath(pkiDir))
	if err != nil {
		return nil, fmt.Errorf("read CA cert: %w", err)
	}
	return &NodeBundle{CertPEM: cert, KeyPEM: key, CACertPEM: caCert}, nil
}

// ClientTLSConfig builds a *tls.Config that presents the plugin's bundle as
// the client identity and verifies the server cert against bundle.CACertPEM.
//
// ServerName is "core.orca.local" — matches the SAN the host's server cert is
// issued with by `pki::init`.
func ClientTLSConfig(bundle *NodeBundle) (*tls.Config, error) {
	cert, err := tls.X509KeyPair(bundle.CertPEM, bundle.KeyPEM)
	if err != nil {
		return nil, fmt.Errorf("parse plugin cert/key pair: %w", err)
	}

	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(bundle.CACertPEM) {
		return nil, errors.New("CA PEM contained no parseable certificates")
	}

	return &tls.Config{
		Certificates: []tls.Certificate{cert},
		RootCAs:      roots,
		ServerName:   "core.orca.local",
		MinVersion:   tls.VersionTLS13,
	}, nil
}
