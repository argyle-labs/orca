package manifest

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeFile(path, content string) error {
	return os.WriteFile(path, []byte(content), 0o600)
}

const canonical = `
[plugin]
id               = "alpha"
version          = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./bin/alpha"
mode   = "process"
eager  = false

[surfaces]
mcp = true

[[capabilities]]
name        = "context.publish"
sensitivity = "general"

[[capabilities]]
name        = "atlassian.read"
sensitivity = "sensitive"
`

func TestParsesCanonicalFixture(t *testing.T) {
	m, err := ParseString(canonical)
	if err != nil {
		t.Fatalf("ParseString: %v", err)
	}
	if m.Plugin.ID != "alpha" {
		t.Errorf("plugin.id = %q, want alpha", m.Plugin.ID)
	}
	if m.Runtime.Binary == nil || *m.Runtime.Binary != "./bin/alpha" {
		t.Errorf("runtime.binary not parsed")
	}
	if m.Runtime.Mode != "process" {
		t.Errorf("runtime.mode = %q", m.Runtime.Mode)
	}
	if !m.Surfaces.MCP {
		t.Errorf("surfaces.mcp should be true")
	}
	if len(m.Capabilities) != 2 {
		t.Fatalf("expected 2 capabilities, got %d", len(m.Capabilities))
	}
	if m.Capabilities[1].Sensitivity != "sensitive" {
		t.Errorf("second capability sensitivity = %q", m.Capabilities[1].Sensitivity)
	}
}

func TestRejectsUnknownTopLevelField(t *testing.T) {
	s := canonical + "\n[bogus]\nx = 1\n"
	_, err := ParseString(s)
	if err == nil || !strings.Contains(err.Error(), "strict") {
		t.Fatalf("expected strict-mode error, got %v", err)
	}
}

func TestRejectsBothBinaryAndImage(t *testing.T) {
	s := `
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
image  = "ghcr.io/x:1"
`
	_, err := ParseString(s)
	if err == nil || !strings.Contains(err.Error(), "mutually exclusive") {
		t.Fatalf("expected mutually-exclusive error, got %v", err)
	}
}

func TestRejectsNeitherBinaryNorImage(t *testing.T) {
	s := `
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
`
	_, err := ParseString(s)
	if err == nil || !strings.Contains(err.Error(), "binary") {
		t.Fatalf("expected binary/image error, got %v", err)
	}
}

func TestRejectsBadID(t *testing.T) {
	for _, bad := range []string{"", "has space", "has/slash", "has\\slash"} {
		s := `
[plugin]
id = "` + bad + `"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
`
		if _, err := ParseString(s); err == nil {
			t.Errorf("expected error for id %q", bad)
		}
	}
}

func TestRejectsBadSemver(t *testing.T) {
	s := `
[plugin]
id = "x"
version = "0.1.0-rc1"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
`
	_, err := ParseString(s)
	if err == nil || !strings.Contains(err.Error(), "pre-release") {
		t.Fatalf("expected pre-release error, got %v", err)
	}
}

func TestRejectsDuplicateCapabilities(t *testing.T) {
	s := `
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"

[[capabilities]]
name        = "thing"
sensitivity = "general"

[[capabilities]]
name        = "thing"
sensitivity = "sensitive"
`
	_, err := ParseString(s)
	if err == nil || !strings.Contains(err.Error(), "duplicate") {
		t.Fatalf("expected duplicate error, got %v", err)
	}
}

func TestSurfacesDefaultFalse(t *testing.T) {
	s := `
[plugin]
id = "x"
version = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./b"
`
	m, err := ParseString(s)
	if err != nil {
		t.Fatalf("ParseString: %v", err)
	}
	if m.Surfaces.MCP || m.Surfaces.Federation {
		t.Errorf("surfaces should default false")
	}
}

func TestParseFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, Filename)
	if err := writeFile(path, canonical); err != nil {
		t.Fatalf("write: %v", err)
	}
	m, err := ParseFile(path)
	if err != nil {
		t.Fatalf("ParseFile: %v", err)
	}
	if m.Plugin.ID != "alpha" {
		t.Errorf("expected alpha, got %q", m.Plugin.ID)
	}
}
