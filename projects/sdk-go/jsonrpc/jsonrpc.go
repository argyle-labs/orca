// Package jsonrpc holds the JSON-RPC 2.0 wire types shared between the
// orca plugin SDK and the orca plugin host.
//
// Wire-compatible with projects/sdk/src/jsonrpc.rs (the Rust reference).
// Field names, optional-field omission rules, and the untagged Message
// envelope are all chosen to round-trip across implementations.
package jsonrpc

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
)

// Version is the JSON-RPC version string we always emit and require.
const Version = "2.0"

// Request is a JSON-RPC 2.0 request message.
type Request struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// NewRequest builds a Request with the standard "2.0" version. id and params
// must already be JSON-encoded; the caller decides whether id is a number,
// string, or null.
func NewRequest(id json.RawMessage, method string, params json.RawMessage) Request {
	return Request{JSONRPC: Version, ID: id, Method: method, Params: params}
}

// Notification is a JSON-RPC 2.0 notification (no id, no response expected).
type Notification struct {
	JSONRPC string          `json:"jsonrpc"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// NewNotification builds a Notification with the standard version string.
func NewNotification(method string, params json.RawMessage) Notification {
	return Notification{JSONRPC: Version, Method: method, Params: params}
}

// ErrorObject is a JSON-RPC 2.0 error payload.
type ErrorObject struct {
	Code    int64           `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

// Standard JSON-RPC 2.0 error codes used by the orca plugin host.
const (
	CodeMethodNotFound = -32601
	CodeInvalidParams  = -32602
	CodeInternalError  = -32603
)

// MethodNotFound builds a -32601 error referring to the missing method.
func MethodNotFound(method string) ErrorObject {
	return ErrorObject{Code: CodeMethodNotFound, Message: "method not found: " + method}
}

// InvalidParams builds a -32602 error with the supplied detail.
func InvalidParams(detail string) ErrorObject {
	return ErrorObject{Code: CodeInvalidParams, Message: "invalid params: " + detail}
}

// Internal builds a -32603 error with the supplied detail.
func Internal(detail string) ErrorObject {
	return ErrorObject{Code: CodeInternalError, Message: "internal error: " + detail}
}

// Response is a JSON-RPC 2.0 response message. Exactly one of Result or Error
// is set; both are encoded with `omitempty` so the wire form matches the Rust
// reference.
type Response struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Result  json.RawMessage `json:"result,omitempty"`
	Error   *ErrorObject    `json:"error,omitempty"`
}

// OK builds a successful Response.
func OK(id json.RawMessage, result json.RawMessage) Response {
	return Response{JSONRPC: Version, ID: id, Result: result}
}

// Err builds a failure Response.
func Err(id json.RawMessage, e ErrorObject) Response {
	return Response{JSONRPC: Version, ID: id, Error: &e}
}

// IsError reports whether r carries an error payload.
func (r Response) IsError() bool { return r.Error != nil }

// MessageKind tags the variant returned by ParseMessage. The Rust SDK uses
// serde's untagged enum; in Go we dispatch on shape and return a tag.
type MessageKind int

const (
	KindUnknown MessageKind = iota
	KindRequest
	KindNotification
	KindResponse
)

// Message is the discriminated form of a frame received from the wire.
// Only the field corresponding to Kind is meaningful.
type Message struct {
	Kind         MessageKind
	Request      Request
	Notification Notification
	Response     Response
}

// ErrUnknownMessage is returned when ParseMessage cannot classify a payload
// as request, notification, or response.
var ErrUnknownMessage = errors.New("jsonrpc: unrecognized message shape")

// ParseMessage classifies a JSON-RPC frame as request, notification, or
// response by inspecting which fields are present:
//
//   - has "method" and "id"            → Request
//   - has "method" but no "id"         → Notification
//   - has "id" and ("result" | "error")→ Response
//
// The Rust reference (`Message` untagged enum) accepts the same shapes; the
// dispatch order matches `#[serde(untagged)]` Response → Notification → Request.
func ParseMessage(data []byte) (Message, error) {
	var probe struct {
		Method *string         `json:"method"`
		ID     json.RawMessage `json:"id"`
		Result json.RawMessage `json:"result"`
		Error  json.RawMessage `json:"error"`
	}
	dec := json.NewDecoder(bytes.NewReader(data))
	if err := dec.Decode(&probe); err != nil {
		return Message{}, fmt.Errorf("jsonrpc: parse: %w", err)
	}

	hasMethod := probe.Method != nil
	hasID := len(probe.ID) > 0
	hasResultOrErr := len(probe.Result) > 0 || len(probe.Error) > 0

	switch {
	case hasID && hasResultOrErr && !hasMethod:
		var r Response
		if err := json.Unmarshal(data, &r); err != nil {
			return Message{}, fmt.Errorf("jsonrpc: response decode: %w", err)
		}
		return Message{Kind: KindResponse, Response: r}, nil
	case hasMethod && !hasID:
		var n Notification
		if err := json.Unmarshal(data, &n); err != nil {
			return Message{}, fmt.Errorf("jsonrpc: notification decode: %w", err)
		}
		return Message{Kind: KindNotification, Notification: n}, nil
	case hasMethod && hasID:
		var req Request
		if err := json.Unmarshal(data, &req); err != nil {
			return Message{}, fmt.Errorf("jsonrpc: request decode: %w", err)
		}
		return Message{Kind: KindRequest, Request: req}, nil
	default:
		return Message{}, ErrUnknownMessage
	}
}
