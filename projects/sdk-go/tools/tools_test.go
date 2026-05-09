package tools

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
)

func TestDeclarationRoundtrips(t *testing.T) {
	d := ToolDeclaration{
		Name:        "stack.list",
		Description: "List Dockge stacks",
		InputSchema: json.RawMessage(`{"type":"object","properties":{}}`),
		Sensitivity: SensitivityGeneral,
	}
	b, err := json.Marshal(d)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var back ToolDeclaration
	if err := json.Unmarshal(b, &back); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if back.Name != "stack.list" || back.Sensitivity != SensitivityGeneral {
		t.Fatalf("roundtrip mismatch: %+v", back)
	}
}

func TestHandlerErrorIsError(t *testing.T) {
	var err error = NewHandlerError("upstream rejected")
	if err.Error() == "" {
		t.Fatal("Error() should produce a message")
	}
	var herr *HandlerError
	if !errors.As(err, &herr) {
		t.Fatalf("errors.As should pick up *HandlerError")
	}
	if herr.Message != "upstream rejected" {
		t.Fatalf("message lost: %q", herr.Message)
	}
}

func TestHandlerSignature(t *testing.T) {
	// Confirm a closure satisfies the Handler type.
	var h Handler = func(_ context.Context, args json.RawMessage) (json.RawMessage, error) {
		return args, nil
	}
	out, err := h(context.Background(), json.RawMessage(`{"k":1}`))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if string(out) != `{"k":1}` {
		t.Fatalf("got %s", out)
	}
}
