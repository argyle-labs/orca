// orca-conformance-plugin (Go) — companion to projects/conformance-plugin in
// Rust. Exercises the same scenario through the Go SDK so the two binaries
// can be diffed against the same conformance host.
//
// Reads four env vars set by orca_sdk::conformance::run_subprocess:
//
//	ORCA_PLUGIN_ADDR    — host:port of the conformance host (TCP+mTLS)
//	ORCA_PKI_DIR        — directory holding CA + this plugin's cert/key
//	ORCA_PLUGIN_ID      — id to claim in orca/hello (matches cert CN)
//	ORCA_MANIFEST_PATH  — path to the canonical manifest fixture
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"orca/sdk-go/manifest"
	"orca/sdk-go/pki"
	"orca/sdk-go/transport"
)

const (
	scenarioTypeName             = "Greeting"
	scenarioTypeSchemaVersion    = "0.1.0"
	scenarioContextID            = "conformance:hello"
	scenarioManifestIDPayloadKey = "manifest_id"

	// Tools surface — must match projects/sdk/src/conformance.rs SCENARIO.
	scenarioToolName          = "echo"
	scenarioToolArgKey        = "value"
	scenarioToolArgValue      = "ping" // host invokes with this value
	scenarioToolResultEchoKey = "echoed"
)

const scenarioTypeSchema = `{"type":"object","properties":{"text":{"type":"string"},"manifest_id":{"type":"string"}},"required":["text","manifest_id"]}`

const scenarioToolInputSchema = `{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}`

func envRequired(name string) (string, error) {
	v := os.Getenv(name)
	if v == "" {
		return "", fmt.Errorf("required env var %s not set", name)
	}
	return v, nil
}

func run() error {
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
	manifestPath, err := envRequired("ORCA_MANIFEST_PATH")
	if err != nil {
		return err
	}

	mf, err := manifest.ParseFile(manifestPath)
	if err != nil {
		return fmt.Errorf("parse manifest: %w", err)
	}
	if mf.Plugin.ID != pluginID {
		return fmt.Errorf("manifest plugin.id %q != ORCA_PLUGIN_ID %q", mf.Plugin.ID, pluginID)
	}

	bundle, err := pki.LoadPlugin(pkiDir, pluginID)
	if err != nil {
		return fmt.Errorf("load plugin bundle: %w", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	tr, err := transport.Connect(ctx, addr, bundle)
	if err != nil {
		return fmt.Errorf("connect to conformance host: %w", err)
	}
	defer tr.Close()

	if _, err := tr.Hello(ctx, pluginID, transport.FlavorHeadless, nil, nil); err != nil {
		return fmt.Errorf("orca/hello: %w", err)
	}

	if _, err := tr.DeclareTypes(ctx, []transport.TypeDeclaration{{
		TypeName:      scenarioTypeName,
		SchemaVersion: scenarioTypeSchemaVersion,
		Schema:        json.RawMessage(scenarioTypeSchema),
		Sensitivity:   transport.SensitivityGeneral,
	}}); err != nil {
		return fmt.Errorf("orca/types.declare: %w", err)
	}

	payload, err := json.Marshal(map[string]string{
		"text":                       "hello from the Go conformance plugin",
		scenarioManifestIDPayloadKey: mf.Plugin.ID,
	})
	if err != nil {
		return fmt.Errorf("encode payload: %w", err)
	}

	if err := tr.PublishContext(ctx, scenarioContextID, transport.TypedValue{
		TypeID:        fmt.Sprintf("%s.%s", pluginID, scenarioTypeName),
		SchemaVersion: scenarioTypeSchemaVersion,
		Sensitivity:   transport.SensitivityGeneral,
		Payload:       payload,
	}); err != nil {
		return fmt.Errorf("orca/context.publish: %w", err)
	}

	// Register the echo tool and declare it. The host calls back with
	// orca/tools.call after seeing the declaration; the read loop dispatches
	// to the handler below and writes the response on this same connection.
	tr.RegisterTool(
		scenarioToolName,
		"echo back the value argument",
		json.RawMessage(scenarioToolInputSchema),
		transport.SensitivityGeneral,
		func(_ context.Context, args json.RawMessage) (json.RawMessage, error) {
			var in map[string]string
			if err := json.Unmarshal(args, &in); err != nil {
				return nil, &transport.ToolHandlerError{Message: "decode args: " + err.Error()}
			}
			v, ok := in[scenarioToolArgKey]
			if !ok {
				return nil, &transport.ToolHandlerError{Message: "missing 'value' arg"}
			}
			out, err := json.Marshal(map[string]string{scenarioToolResultEchoKey: v})
			if err != nil {
				return nil, &transport.ToolHandlerError{Message: err.Error()}
			}
			return out, nil
		},
	)
	if _, err := tr.DeclareTools(ctx); err != nil {
		return fmt.Errorf("orca/tools.declare: %w", err)
	}

	// Hold the connection open so the host's tools.call can land and the
	// read loop can serve the response. The conformance harness drives
	// completion by observing all 5 events; we just need to outlive that.
	time.Sleep(2 * time.Second)
	return nil
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "orca-conformance-plugin (go):", err)
		os.Exit(1)
	}
}
