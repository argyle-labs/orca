// Package manifest parses orca-plugin.toml. Wire-compatible with
// projects/sdk/src/manifest.rs (the Rust reference). Every language SDK port
// must accept the canonical fixture verbatim.
package manifest

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

// Filename is the conventional manifest filename.
const Filename = "orca-plugin.toml"

// Manifest is the parsed top-level orca-plugin.toml.
type Manifest struct {
	Plugin       PluginSection    `toml:"plugin"`
	Runtime      RuntimeSection   `toml:"runtime"`
	Surfaces     SurfacesSection  `toml:"surfaces"`
	Capabilities []CapabilityDecl `toml:"capabilities"`
}

// PluginSection — `[plugin]`.
type PluginSection struct {
	ID             string `toml:"id"`
	Version        string `toml:"version"`
	MinOrcaVersion string `toml:"min_orca_version"`
}

// RuntimeSection — `[runtime]`. Exactly one of Binary / Image must be set.
type RuntimeSection struct {
	Binary *string `toml:"binary"`
	Image  *string `toml:"image"`
	Mode   string  `toml:"mode"`
	Eager  bool    `toml:"eager"`
}

// SurfacesSection — `[surfaces]`. All fields default to false.
type SurfacesSection struct {
	MCP        bool `toml:"mcp"`
	CLI        bool `toml:"cli"`
	UI         bool `toml:"ui"`
	Docs       bool `toml:"docs"`
	Jobs       bool `toml:"jobs"`
	Storage    bool `toml:"storage"`
	Federation bool `toml:"federation"`
}

// CapabilityDecl — one entry under `[[capabilities]]`. Sensitivity must be
// "general" or "sensitive" (matches projects/sdk/src/pki.rs::Capability).
type CapabilityDecl struct {
	Name        string `toml:"name"`
	Sensitivity string `toml:"sensitivity"`
}

// ParseString parses a TOML manifest from a string.
func ParseString(s string) (*Manifest, error) {
	dec := toml.NewDecoder(bytes.NewBufferString(s))
	dec.DisallowUnknownFields()

	var m Manifest
	if err := dec.Decode(&m); err != nil {
		return nil, fmt.Errorf("parse %s: %w", Filename, err)
	}
	if m.Runtime.Mode == "" {
		m.Runtime.Mode = "process"
	}
	if err := m.validate(); err != nil {
		return nil, err
	}
	return &m, nil
}

// ParseFile parses a TOML manifest from disk.
func ParseFile(path string) (*Manifest, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read manifest at %s: %w", path, err)
	}
	return ParseString(string(data))
}

func (m *Manifest) validate() error {
	if strings.TrimSpace(m.Plugin.ID) == "" {
		return errors.New("plugin.id must not be empty")
	}
	for _, r := range m.Plugin.ID {
		if r == ' ' || r == '\t' || r == '\n' || r == '/' || r == '\\' {
			return fmt.Errorf("plugin.id %q contains invalid characters (whitespace or path separators)", m.Plugin.ID)
		}
	}
	if err := checkSemver(m.Plugin.Version, "plugin.version"); err != nil {
		return err
	}
	if err := checkSemver(m.Plugin.MinOrcaVersion, "plugin.min_orca_version"); err != nil {
		return err
	}

	switch {
	case m.Runtime.Binary != nil && m.Runtime.Image != nil:
		return errors.New("runtime.binary and runtime.image are mutually exclusive")
	case m.Runtime.Binary == nil && m.Runtime.Image == nil:
		return errors.New("runtime requires either `binary` or `image`")
	}
	if m.Runtime.Mode != "process" {
		return fmt.Errorf("runtime.mode %q: only \"process\" is supported in v0", m.Runtime.Mode)
	}

	seen := make(map[string]struct{}, len(m.Capabilities))
	for _, c := range m.Capabilities {
		if strings.TrimSpace(c.Name) == "" {
			return errors.New("capability.name must not be empty")
		}
		if c.Sensitivity != "general" && c.Sensitivity != "sensitive" {
			return fmt.Errorf("capability %q: sensitivity must be \"general\" or \"sensitive\", got %q", c.Name, c.Sensitivity)
		}
		if _, dup := seen[c.Name]; dup {
			return fmt.Errorf("duplicate capability %q", c.Name)
		}
		seen[c.Name] = struct{}{}
	}
	return nil
}

func checkSemver(v, field string) error {
	if strings.ContainsAny(v, "-+") {
		return fmt.Errorf("%s %q: pre-release/build metadata not supported in v0", field, v)
	}
	parts := strings.Split(v, ".")
	if len(parts) == 0 {
		return fmt.Errorf("%s %q: empty", field, v)
	}
	for _, p := range parts {
		if p == "" {
			return fmt.Errorf("%s %q: empty component", field, v)
		}
		if _, err := strconv.ParseUint(p, 10, 64); err != nil {
			return fmt.Errorf("%s %q: bad numeric component %q", field, v, p)
		}
	}
	return nil
}
