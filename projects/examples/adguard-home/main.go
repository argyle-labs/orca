// adguard-home — orca plugin example.
//
// Polls an AdGuard Home instance's /control/stats endpoint and publishes
// the rolling 24h DNS query stats into the `network:dns` context.
//
// Required env (in addition to the four ORCA_* the host injects):
//
//	ADGUARD_BASE_URL    e.g. http://192.168.1.1:3000
//	ADGUARD_USERNAME    Admin user
//	ADGUARD_PASSWORD    Admin password
//
// Standalone dev:
//
//	ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
//	ORCA_PKI_DIR=$HOME/.orca/pki \
//	ORCA_PLUGIN_ID=adguard-home \
//	ADGUARD_BASE_URL=http://10.0.0.2:3000 \
//	ADGUARD_USERNAME=admin ADGUARD_PASSWORD=changeme \
//	go run .
//
// API reference: https://github.com/AdguardTeam/AdGuardHome/wiki/API
package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"time"

	"github.com/scottdkey/orca/projects/sdk-go/pki"
	"github.com/scottdkey/orca/projects/sdk-go/transport"
)

const (
	typeName      = "DnsQueryStats"
	schemaVersion = "0.1.0"
	contextID     = "network:dns"
	pollInterval  = 30 * time.Second
)

const schema = `{
  "type": "object",
  "properties": {
    "host": { "type": "string" },
    "num_dns_queries": { "type": "integer" },
    "num_blocked_filtering": { "type": "integer" },
    "num_replaced_safebrowsing": { "type": "integer" },
    "num_replaced_parental": { "type": "integer" },
    "avg_processing_time": { "type": "number" }
  },
  "required": ["host", "num_dns_queries"]
}`

type statsResp struct {
	NumDnsQueries           int     `json:"num_dns_queries"`
	NumBlockedFiltering     int     `json:"num_blocked_filtering"`
	NumReplacedSafebrowsing int     `json:"num_replaced_safebrowsing"`
	NumReplacedParental     int     `json:"num_replaced_parental"`
	AvgProcessingTime       float64 `json:"avg_processing_time"`
}

func envRequired(name string) (string, error) {
	v := os.Getenv(name)
	if v == "" {
		return "", fmt.Errorf("required env var %s not set", name)
	}
	return v, nil
}

func fetchStats(client *http.Client, baseURL, user, pass string) (*statsResp, error) {
	u, err := url.JoinPath(baseURL, "/control/stats")
	if err != nil {
		return nil, fmt.Errorf("join url: %w", err)
	}
	req, err := http.NewRequest(http.MethodGet, u, nil)
	if err != nil {
		return nil, err
	}
	auth := base64.StdEncoding.EncodeToString([]byte(user + ":" + pass))
	req.Header.Set("Authorization", "Basic "+auth)
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("GET %s: %w", u, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 512))
		return nil, fmt.Errorf("adguard %d: %s", resp.StatusCode, string(body))
	}
	var s statsResp
	if err := json.NewDecoder(resp.Body).Decode(&s); err != nil {
		return nil, fmt.Errorf("decode stats: %w", err)
	}
	return &s, nil
}

func run(ctx context.Context) error {
	addr, err := envRequired("ORCA_PLUGIN_ADDR")
	if err != nil {
		return err
	}
	pkiDir, err := envRequired("ORCA_PKI_DIR")
	if err != nil {
		return err
	}
	pluginID, err := envRequired("ORCA_PLUGIN_ID")
	if err != nil {
		return err
	}
	baseURL, err := envRequired("ADGUARD_BASE_URL")
	if err != nil {
		return err
	}
	user, err := envRequired("ADGUARD_USERNAME")
	if err != nil {
		return err
	}
	pass, err := envRequired("ADGUARD_PASSWORD")
	if err != nil {
		return err
	}

	bundle, err := pki.LoadPlugin(pkiDir, pluginID)
	if err != nil {
		return fmt.Errorf("load bundle: %w", err)
	}
	tr, err := transport.Connect(ctx, addr, bundle)
	if err != nil {
		return fmt.Errorf("connect: %w", err)
	}
	defer tr.Close()

	if _, err := tr.Hello(ctx, pluginID, transport.FlavorHeadless, nil, nil); err != nil {
		return fmt.Errorf("hello: %w", err)
	}

	if _, err := tr.DeclareTypes(ctx, []transport.TypeDeclaration{{
		TypeName:      typeName,
		SchemaVersion: schemaVersion,
		Schema:        json.RawMessage(schema),
		Sensitivity:   transport.SensitivityGeneral,
	}}); err != nil {
		return fmt.Errorf("declare: %w", err)
	}

	httpClient := &http.Client{Timeout: 10 * time.Second}
	host, _ := os.Hostname()
	typeID := fmt.Sprintf("%s.%s", pluginID, typeName)

	ticker := time.NewTicker(pollInterval)
	defer ticker.Stop()
	publish := func() error {
		stats, err := fetchStats(httpClient, baseURL, user, pass)
		if err != nil {
			fmt.Fprintln(os.Stderr, "adguard fetch:", err)
			return nil // soft-fail; keep the plugin alive across transient outages
		}
		payload, err := json.Marshal(map[string]any{
			"host":                       host,
			"num_dns_queries":            stats.NumDnsQueries,
			"num_blocked_filtering":      stats.NumBlockedFiltering,
			"num_replaced_safebrowsing":  stats.NumReplacedSafebrowsing,
			"num_replaced_parental":      stats.NumReplacedParental,
			"avg_processing_time":        stats.AvgProcessingTime,
		})
		if err != nil {
			return fmt.Errorf("encode payload: %w", err)
		}
		return tr.PublishContext(ctx, contextID, transport.TypedValue{
			TypeID:        typeID,
			SchemaVersion: schemaVersion,
			Sensitivity:   transport.SensitivityGeneral,
			Payload:       payload,
		})
	}

	if err := publish(); err != nil {
		return err
	}
	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
			if err := publish(); err != nil {
				return err
			}
		}
	}
}

func main() {
	if err := run(context.Background()); err != nil {
		fmt.Fprintln(os.Stderr, "orca-example-adguard-home:", err)
		os.Exit(1)
	}
}
