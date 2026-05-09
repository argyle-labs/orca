// Package tools is the Go port of the orca SDK's tools surface.
// Wire-compatible with projects/sdk/src/tools.rs.
//
// Plugins declare callable tools via orca/tools.declare and serve them
// via orca/tools.call. The host owns the namespace — plugins register
// bare names like "stack.list"; the host registers them as
// "<plugin_id>.stack.list".
package tools

import (
	"context"
	"encoding/json"
	"fmt"
)

// Method names exchanged on the wire. Constants so callers don't have to
// remember the exact strings.
const (
	DeclareMethod = "orca/tools.declare"
	CallMethod    = "orca/tools.call"
)

// JSON-RPC error codes specific to the tools surface. These extend the
// standard -32600..-32099 range. Match projects/sdk/src/tools.rs.
const (
	// ErrCodeUnknownTool — the named tool is not registered.
	ErrCodeUnknownTool = -32001
	// ErrCodeSchemaViolation — arguments did not match the declared input_schema.
	ErrCodeSchemaViolation = -32002
	// ErrCodeHandlerError — handler ran but returned an application error.
	ErrCodeHandlerError = -32003
)

// Sensitivity is duplicated here rather than imported from transport to
// keep the tools package free of cycles. Callers pass through values
// declared on the transport.
type Sensitivity string

const (
	SensitivityGeneral   Sensitivity = "general"
	SensitivitySensitive Sensitivity = "sensitive"
)

// ToolDeclaration is one tool the plugin announces. The fully-qualified id
// is computed host-side as "<plugin_id>.<name>".
type ToolDeclaration struct {
	Name        string          `json:"name"`
	Description string          `json:"description"`
	InputSchema json.RawMessage `json:"input_schema"`
	Sensitivity Sensitivity     `json:"sensitivity"`
}

// ToolsDeclareParams is the wire shape of orca/tools.declare params.
type ToolsDeclareParams struct {
	Tools []ToolDeclaration `json:"tools"`
}

// ToolsDeclareResult lists the namespaced ids the host registered.
type ToolsDeclareResult struct {
	Accepted []string `json:"accepted"`
}

// ToolCallParams is the wire shape of orca/tools.call params. Name is the
// bare tool name (no plugin_id prefix — the host strips it before dispatch).
type ToolCallParams struct {
	Name      string          `json:"name"`
	Arguments json.RawMessage `json:"arguments"`
}

// ToolCallResult is the wire shape of orca/tools.call result. Opaque JSON.
type ToolCallResult struct {
	Result json.RawMessage `json:"result"`
}

// HandlerError is the application-level error a tool handler can return.
// The transport translates this into a JSON-RPC error response with code
// ErrCodeHandlerError. Implements the error interface.
type HandlerError struct {
	Message string
	Data    json.RawMessage
}

// NewHandlerError constructs a HandlerError with no extra data.
func NewHandlerError(msg string) *HandlerError {
	return &HandlerError{Message: msg}
}

// NewHandlerErrorWithData constructs a HandlerError carrying structured
// detail the caller can deserialize.
func NewHandlerErrorWithData(msg string, data json.RawMessage) *HandlerError {
	return &HandlerError{Message: msg, Data: data}
}

func (e *HandlerError) Error() string {
	return fmt.Sprintf("tool handler error: %s", e.Message)
}

// Handler is the function shape every tool implementation satisfies. Return
// a *HandlerError to surface an application-level failure with the
// HANDLER_ERROR code; any other error is treated as an internal error.
type Handler func(ctx context.Context, args json.RawMessage) (json.RawMessage, error)

// RegisteredTool bundles a declaration with its handler. Stored in the
// transport's per-connection registry; not part of the wire format.
type RegisteredTool struct {
	Declaration ToolDeclaration
	Handler     Handler
}
