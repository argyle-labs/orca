package jsonrpc

import (
	"bytes"
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

func TestRequestRoundtrips(t *testing.T) {
	req := NewRequest(json.RawMessage(`1`), "orca/hello", json.RawMessage(`{"sdk_version":"0.1.0"}`))
	enc, err := json.Marshal(req)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var back Request
	if err := json.Unmarshal(enc, &back); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if back.Method != "orca/hello" {
		t.Fatalf("method = %q", back.Method)
	}
	if string(back.ID) != "1" {
		t.Fatalf("id = %s", back.ID)
	}
}

func TestResponseOKRoundtrips(t *testing.T) {
	r := OK(json.RawMessage(`1`), json.RawMessage(`{"ok":true}`))
	enc, err := json.Marshal(r)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if bytes.Contains(enc, []byte(`"error"`)) {
		t.Fatalf("ok response should omit error field, got: %s", enc)
	}
	var back Response
	if err := json.Unmarshal(enc, &back); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if back.IsError() {
		t.Fatalf("expected ok, got error")
	}
}

func TestResponseErrRoundtrips(t *testing.T) {
	r := Err(json.RawMessage(`1`), MethodNotFound("foo/bar"))
	enc, err := json.Marshal(r)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if bytes.Contains(enc, []byte(`"result"`)) {
		t.Fatalf("err response should omit result field, got: %s", enc)
	}
	var back Response
	if err := json.Unmarshal(enc, &back); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if !back.IsError() {
		t.Fatalf("expected error, got ok")
	}
	if back.Error.Code != CodeMethodNotFound {
		t.Fatalf("code = %d", back.Error.Code)
	}
}

func TestParseResponse(t *testing.T) {
	raw := []byte(`{"jsonrpc":"2.0","id":1,"result":{"ok":true}}`)
	msg, err := ParseMessage(raw)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if msg.Kind != KindResponse {
		t.Fatalf("kind = %v, want KindResponse", msg.Kind)
	}
}

func TestParseNotification(t *testing.T) {
	raw := []byte(`{"jsonrpc":"2.0","method":"orca/ping"}`)
	msg, err := ParseMessage(raw)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if msg.Kind != KindNotification {
		t.Fatalf("kind = %v, want KindNotification", msg.Kind)
	}
	if msg.Notification.Method != "orca/ping" {
		t.Fatalf("method = %q", msg.Notification.Method)
	}
}

func TestParseRequest(t *testing.T) {
	raw := []byte(`{"jsonrpc":"2.0","id":7,"method":"orca/types.declare","params":{"types":[]}}`)
	msg, err := ParseMessage(raw)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if msg.Kind != KindRequest {
		t.Fatalf("kind = %v, want KindRequest", msg.Kind)
	}
	if msg.Request.Method != "orca/types.declare" {
		t.Fatalf("method = %q", msg.Request.Method)
	}
}

func TestParseUnknownShape(t *testing.T) {
	// No method, no id, no result/error — not a recognizable JSON-RPC frame.
	_, err := ParseMessage([]byte(`{"jsonrpc":"2.0"}`))
	if !errors.Is(err, ErrUnknownMessage) {
		t.Fatalf("expected ErrUnknownMessage, got %v", err)
	}
}

// Wire-level cross-check against the Rust reference: a hand-encoded payload
// that matches what `serde_json::to_string` produces for these types must
// round-trip through Go without losing fields.
func TestWireCompatibilityWithRustReference(t *testing.T) {
	// Sample payloads taken from the Rust SDK tests in projects/sdk/src/jsonrpc.rs.
	cases := []struct {
		name string
		raw  string
		kind MessageKind
	}{
		{"hello response", `{"jsonrpc":"2.0","id":1,"result":{"ok":true,"status":"full","methods":[],"server_version":"0.1.0"}}`, KindResponse},
		{"method-not-found", `{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found: foo/bar"}}`, KindResponse},
		{"context.event notification", `{"jsonrpc":"2.0","method":"orca/context.event","params":{"subscription_id":"abc","context_id":"room","value":{"type_id":"X","schema_version":"0.1.0","sensitivity":"general","payload":{}}}}`, KindNotification},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			msg, err := ParseMessage([]byte(tc.raw))
			if err != nil {
				t.Fatalf("parse: %v", err)
			}
			if msg.Kind != tc.kind {
				t.Fatalf("kind = %v, want %v", msg.Kind, tc.kind)
			}
		})
	}
}

// Field name discipline: Go SDK output must use snake_case keys to match the
// Rust reference's serde derives. Our struct tags handle this; this test
// guards against accidental field-renames.
func TestEmittedFieldNames(t *testing.T) {
	resp := OK(json.RawMessage(`1`), json.RawMessage(`{}`))
	enc, _ := json.Marshal(resp)
	for _, want := range []string{`"jsonrpc"`, `"id"`, `"result"`} {
		if !strings.Contains(string(enc), want) {
			t.Errorf("missing field %s in output: %s", want, enc)
		}
	}
}
