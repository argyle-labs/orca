// Package transport is the plugin (client) side of the orca TCP+mTLS
// transport. Wire-compatible with projects/sdk/src/transport.rs.
//
// A Transport wraps one mTLS-authenticated TLS stream. A background reader
// goroutine demuxes incoming frames:
//   - Responses are routed to the matching Call() goroutine via a per-id
//     reply channel kept in the demux table.
//   - Notifications are fanned out on a broadcast channel; callers subscribe
//     via Notifications() (or higher-level helpers like SubscribeContext).
package transport

import (
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"orca/sdk-go/framing"
	"orca/sdk-go/jsonrpc"
	"orca/sdk-go/pki"
	"orca/sdk-go/tools"
)

// Re-exported tools surface so plugin authors only need to import
// "orca/sdk-go/transport". Mirrors the Rust SDK's lib.rs re-exports.
type (
	ToolDeclaration    = tools.ToolDeclaration
	ToolsDeclareParams = tools.ToolsDeclareParams
	ToolsDeclareResult = tools.ToolsDeclareResult
	ToolCallParams     = tools.ToolCallParams
	ToolCallResult     = tools.ToolCallResult
	ToolHandler        = tools.Handler
	ToolHandlerError   = tools.HandlerError
	RegisteredTool     = tools.RegisteredTool
)

// Re-exported tools constants.
const (
	ToolsDeclareMethod      = tools.DeclareMethod
	ToolsCallMethod         = tools.CallMethod
	ToolErrCodeUnknownTool  = tools.ErrCodeUnknownTool
	ToolErrCodeSchemaError  = tools.ErrCodeSchemaViolation
	ToolErrCodeHandlerError = tools.ErrCodeHandlerError
)

// SDKVersion is announced in orca/hello.
const SDKVersion = "0.1.0"

// Flavor matches the Rust `Flavor` enum (lowercase serde rename).
type Flavor string

const (
	FlavorFull     Flavor = "full"
	FlavorHeadless Flavor = "headless"
	FlavorLocal    Flavor = "local"
)

// Sensitivity matches the Rust `Sensitivity` enum.
type Sensitivity string

const (
	SensitivityGeneral   Sensitivity = "general"
	SensitivitySensitive Sensitivity = "sensitive"
)

// HelloParams mirrors projects/sdk/src/transport.rs::HelloParams.
type HelloParams struct {
	SDKVersion      string   `json:"sdk_version"`
	PluginID        string   `json:"plugin_id"`
	Flavor          Flavor   `json:"flavor"`
	CoreMinRequired string   `json:"core_min_required"`
	MethodsRequired []string `json:"methods_required"`
	MethodsOptional []string `json:"methods_optional"`
}

// HelloResult mirrors HelloResult on the Rust side.
type HelloResult struct {
	ServerVersion string   `json:"server_version"`
	OK            bool     `json:"ok"`
	Status        string   `json:"status"`
	Methods       []string `json:"methods"`
	Reason        *string  `json:"reason,omitempty"`
}

// TypeDeclaration matches TypeDeclaration on the Rust side. Schema is raw
// JSON so callers can pass arbitrary JSON Schema documents.
type TypeDeclaration struct {
	TypeName      string          `json:"type_name"`
	SchemaVersion string          `json:"schema_version"`
	Schema        json.RawMessage `json:"schema"`
	Sensitivity   Sensitivity     `json:"sensitivity"`
}

// TypesDeclareResult is the response shape for orca/types.declare.
type TypesDeclareResult struct {
	Accepted []string `json:"accepted"`
}

// TypedValue mirrors the Rust struct. Note `type` is the JSON field name.
type TypedValue struct {
	TypeID        string          `json:"type"`
	SchemaVersion string          `json:"schema_version"`
	Sensitivity   Sensitivity     `json:"sensitivity"`
	Payload       json.RawMessage `json:"payload"`
}

// ContextEvent is the payload of orca/context.event notifications.
type ContextEvent struct {
	SubscriptionID string     `json:"subscription_id"`
	ContextID      string     `json:"context_id"`
	Value          TypedValue `json:"value"`
}

// ContextEventMethod is the JSON-RPC method name the host pushes.
const ContextEventMethod = "orca/context.event"

// Transport is a connected, mTLS-authenticated plugin transport.
type Transport struct {
	conn   net.Conn
	writeM sync.Mutex

	nextID atomic.Uint64

	pendingMu sync.Mutex
	pending   map[uint64]chan jsonrpc.Response

	notifMu     sync.Mutex
	notifSubs   []chan jsonrpc.Notification
	notifClosed bool

	// Tools the plugin has registered for the host to invoke. Keyed by the
	// bare tool name (no plugin_id prefix).
	toolsMu sync.Mutex
	tools   map[string]RegisteredTool

	closeOnce sync.Once
	closed    chan struct{}
}

// Connect dials addr (host:port), performs an mTLS handshake using the
// supplied bundle, and starts the demux goroutine. Cancel ctx to abort.
func Connect(ctx context.Context, addr string, bundle *pki.NodeBundle) (*Transport, error) {
	tlsCfg, err := pki.ClientTLSConfig(bundle)
	if err != nil {
		return nil, err
	}
	dialer := &net.Dialer{Timeout: 10 * time.Second}
	rawConn, err := dialer.DialContext(ctx, "tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("tcp dial %s: %w", addr, err)
	}
	tlsConn := tls.Client(rawConn, tlsCfg)
	if err := tlsConn.HandshakeContext(ctx); err != nil {
		_ = rawConn.Close()
		return nil, fmt.Errorf("tls handshake: %w", err)
	}

	t := &Transport{
		conn:    tlsConn,
		pending: make(map[uint64]chan jsonrpc.Response),
		tools:   make(map[string]RegisteredTool),
		closed:  make(chan struct{}),
	}
	t.nextID.Store(1)
	go t.readLoop()
	return t, nil
}

// Close shuts down the connection. Pending Call()s will fail.
func (t *Transport) Close() error {
	var err error
	t.closeOnce.Do(func() {
		err = t.conn.Close()
		close(t.closed)
		t.failPending(errors.New("transport closed"))
		t.closeNotifs()
	})
	return err
}

func (t *Transport) failPending(reason error) {
	t.pendingMu.Lock()
	defer t.pendingMu.Unlock()
	for id, ch := range t.pending {
		// Push a synthetic error response so blocked callers unblock.
		ch <- jsonrpc.Err(idToRaw(id), jsonrpc.Internal(reason.Error()))
		close(ch)
		delete(t.pending, id)
	}
}

func (t *Transport) closeNotifs() {
	t.notifMu.Lock()
	defer t.notifMu.Unlock()
	if t.notifClosed {
		return
	}
	t.notifClosed = true
	for _, ch := range t.notifSubs {
		close(ch)
	}
	t.notifSubs = nil
}

func (t *Transport) readLoop() {
	defer t.Close()
	for {
		frame, err := framing.Read(t.conn)
		if err != nil {
			return
		}
		msg, err := jsonrpc.ParseMessage(frame)
		if err != nil {
			continue
		}
		switch msg.Kind {
		case jsonrpc.KindResponse:
			id, ok := rawToID(msg.Response.ID)
			if !ok {
				continue
			}
			t.pendingMu.Lock()
			ch, exists := t.pending[id]
			delete(t.pending, id)
			t.pendingMu.Unlock()
			if exists {
				ch <- msg.Response
				close(ch)
			}
		case jsonrpc.KindNotification:
			t.fanout(msg.Notification)
		case jsonrpc.KindRequest:
			// Spawn a goroutine so a slow handler doesn't stall the read loop.
			go t.dispatchIncoming(msg.Request)
		}
	}
}

// dispatchIncoming handles a server→plugin request. Currently only
// orca/tools.call is supported; everything else returns method-not-found.
func (t *Transport) dispatchIncoming(req jsonrpc.Request) {
	resp := t.dispatchOne(req)
	body, err := json.Marshal(resp)
	if err != nil {
		return
	}
	_ = t.writeFrame(body)
}

func (t *Transport) dispatchOne(req jsonrpc.Request) jsonrpc.Response {
	if req.Method != ToolsCallMethod {
		return jsonrpc.Err(req.ID, jsonrpc.MethodNotFound(req.Method))
	}

	if len(req.Params) == 0 {
		return jsonrpc.Err(req.ID, jsonrpc.InvalidParams("missing params"))
	}
	var params ToolCallParams
	if err := json.Unmarshal(req.Params, &params); err != nil {
		return jsonrpc.Err(req.ID, jsonrpc.InvalidParams(err.Error()))
	}

	t.toolsMu.Lock()
	rt, exists := t.tools[params.Name]
	t.toolsMu.Unlock()
	if !exists {
		return jsonrpc.Err(req.ID, jsonrpc.ErrorObject{
			Code:    ToolErrCodeUnknownTool,
			Message: fmt.Sprintf("unknown tool: %s", params.Name),
		})
	}

	ctx := context.Background()
	result, err := rt.Handler(ctx, params.Arguments)
	if err != nil {
		var herr *ToolHandlerError
		if errors.As(err, &herr) {
			return jsonrpc.Err(req.ID, jsonrpc.ErrorObject{
				Code:    ToolErrCodeHandlerError,
				Message: herr.Message,
				Data:    herr.Data,
			})
		}
		return jsonrpc.Err(req.ID, jsonrpc.Internal(err.Error()))
	}

	out := ToolCallResult{Result: result}
	body, err := json.Marshal(out)
	if err != nil {
		return jsonrpc.Err(req.ID, jsonrpc.Internal(err.Error()))
	}
	return jsonrpc.OK(req.ID, body)
}

// RegisterTool registers a tool the host can invoke via orca/tools.call.
// Bare name (no <plugin_id>. prefix — the host applies the namespace).
// Re-registering the same name replaces the previous handler. Call this
// for each tool, then call DeclareTools once to send the batch.
func (t *Transport) RegisterTool(
	name, description string,
	inputSchema json.RawMessage,
	sensitivity Sensitivity,
	handler ToolHandler,
) {
	decl := ToolDeclaration{
		Name:        name,
		Description: description,
		InputSchema: inputSchema,
		Sensitivity: tools.Sensitivity(sensitivity),
	}
	t.toolsMu.Lock()
	t.tools[name] = RegisteredTool{Declaration: decl, Handler: handler}
	t.toolsMu.Unlock()
}

// DeclareTools sends the registered tool set via orca/tools.declare.
// Returns the namespaced ids the host accepted. Idempotent — calling
// again replaces the host-side set.
func (t *Transport) DeclareTools(ctx context.Context) (*ToolsDeclareResult, error) {
	t.toolsMu.Lock()
	decls := make([]ToolDeclaration, 0, len(t.tools))
	for _, rt := range t.tools {
		decls = append(decls, rt.Declaration)
	}
	t.toolsMu.Unlock()

	params := ToolsDeclareParams{Tools: decls}
	resp, err := t.Call(ctx, ToolsDeclareMethod, params)
	if err != nil {
		return nil, err
	}
	if resp.IsError() {
		return nil, fmt.Errorf("%s rejected: %s", ToolsDeclareMethod, resp.Error.Message)
	}
	var result ToolsDeclareResult
	if err := json.Unmarshal(resp.Result, &result); err != nil {
		return nil, fmt.Errorf("decode ToolsDeclareResult: %w", err)
	}
	return &result, nil
}

func (t *Transport) fanout(n jsonrpc.Notification) {
	t.notifMu.Lock()
	subs := append([]chan jsonrpc.Notification(nil), t.notifSubs...)
	t.notifMu.Unlock()
	for _, ch := range subs {
		select {
		case ch <- n:
		default:
			// Slow subscriber — drop. Mirrors broadcast channel behavior on
			// the Rust side where laggers see an explicit Lagged error.
		}
	}
}

// Notifications returns a channel of every server-pushed notification on
// this connection. The channel is closed when the transport is closed.
func (t *Transport) Notifications() <-chan jsonrpc.Notification {
	ch := make(chan jsonrpc.Notification, 64)
	t.notifMu.Lock()
	defer t.notifMu.Unlock()
	if t.notifClosed {
		close(ch)
		return ch
	}
	t.notifSubs = append(t.notifSubs, ch)
	return ch
}

// Call sends a request and waits for the matching response.
func (t *Transport) Call(ctx context.Context, method string, params any) (jsonrpc.Response, error) {
	id := t.nextID.Add(1) - 1
	idRaw := idToRaw(id)

	var paramsRaw json.RawMessage
	if params != nil {
		b, err := json.Marshal(params)
		if err != nil {
			return jsonrpc.Response{}, fmt.Errorf("marshal params: %w", err)
		}
		paramsRaw = b
	}
	req := jsonrpc.NewRequest(idRaw, method, paramsRaw)
	body, err := json.Marshal(req)
	if err != nil {
		return jsonrpc.Response{}, fmt.Errorf("marshal request: %w", err)
	}

	replyCh := make(chan jsonrpc.Response, 1)
	t.pendingMu.Lock()
	t.pending[id] = replyCh
	t.pendingMu.Unlock()

	if err := t.writeFrame(body); err != nil {
		t.pendingMu.Lock()
		delete(t.pending, id)
		t.pendingMu.Unlock()
		return jsonrpc.Response{}, err
	}

	select {
	case resp := <-replyCh:
		return resp, nil
	case <-ctx.Done():
		t.pendingMu.Lock()
		delete(t.pending, id)
		t.pendingMu.Unlock()
		return jsonrpc.Response{}, ctx.Err()
	case <-t.closed:
		return jsonrpc.Response{}, errors.New("transport closed before response arrived")
	}
}

func (t *Transport) writeFrame(body []byte) error {
	t.writeM.Lock()
	defer t.writeM.Unlock()
	return framing.Write(t.conn, body)
}

// Hello performs the orca/hello handshake.
func (t *Transport) Hello(ctx context.Context, pluginID string, flavor Flavor, methodsRequired, methodsOptional []string) (*HelloResult, error) {
	if methodsRequired == nil {
		methodsRequired = []string{}
	}
	if methodsOptional == nil {
		methodsOptional = []string{}
	}
	params := HelloParams{
		SDKVersion:      SDKVersion,
		PluginID:        pluginID,
		Flavor:          flavor,
		CoreMinRequired: "0.1.0",
		MethodsRequired: methodsRequired,
		MethodsOptional: methodsOptional,
	}
	resp, err := t.Call(ctx, "orca/hello", params)
	if err != nil {
		return nil, err
	}
	if resp.IsError() {
		return nil, fmt.Errorf("orca/hello rejected: %s", resp.Error.Message)
	}
	var result HelloResult
	if err := json.Unmarshal(resp.Result, &result); err != nil {
		return nil, fmt.Errorf("decode HelloResult: %w", err)
	}
	if !result.OK {
		reason := "no reason given"
		if result.Reason != nil {
			reason = *result.Reason
		}
		return nil, fmt.Errorf("orca/hello: server returned ok=false (status=%s; %s)", result.Status, reason)
	}
	return &result, nil
}

// DeclareTypes sends orca/types.declare.
func (t *Transport) DeclareTypes(ctx context.Context, types []TypeDeclaration) (*TypesDeclareResult, error) {
	params := struct {
		Types []TypeDeclaration `json:"types"`
	}{Types: types}
	resp, err := t.Call(ctx, "orca/types.declare", params)
	if err != nil {
		return nil, err
	}
	if resp.IsError() {
		return nil, fmt.Errorf("orca/types.declare rejected: %s", resp.Error.Message)
	}
	var result TypesDeclareResult
	if err := json.Unmarshal(resp.Result, &result); err != nil {
		return nil, fmt.Errorf("decode TypesDeclareResult: %w", err)
	}
	return &result, nil
}

// PublishContext sends orca/context.publish.
func (t *Transport) PublishContext(ctx context.Context, contextID string, value TypedValue) error {
	params := struct {
		ContextID string     `json:"context_id"`
		Value     TypedValue `json:"value"`
	}{ContextID: contextID, Value: value}
	resp, err := t.Call(ctx, "orca/context.publish", params)
	if err != nil {
		return err
	}
	if resp.IsError() {
		return fmt.Errorf("orca/context.publish rejected: %s", resp.Error.Message)
	}
	return nil
}

// SubscribeContext sends orca/context.subscribe and returns the
// server-allocated subscription_id plus a channel of matching events.
func (t *Transport) SubscribeContext(ctx context.Context, contextID string, typeFilter []string) (string, <-chan ContextEvent, error) {
	if typeFilter == nil {
		typeFilter = []string{}
	}
	params := struct {
		ContextID  string   `json:"context_id"`
		TypeFilter []string `json:"type_filter"`
	}{ContextID: contextID, TypeFilter: typeFilter}
	resp, err := t.Call(ctx, "orca/context.subscribe", params)
	if err != nil {
		return "", nil, err
	}
	if resp.IsError() {
		return "", nil, fmt.Errorf("orca/context.subscribe rejected: %s", resp.Error.Message)
	}
	var result struct {
		SubscriptionID string `json:"subscription_id"`
	}
	if err := json.Unmarshal(resp.Result, &result); err != nil {
		return "", nil, fmt.Errorf("decode subscribe result: %w", err)
	}

	out := make(chan ContextEvent, 64)
	notifs := t.Notifications()
	go func() {
		defer close(out)
		for n := range notifs {
			if n.Method != ContextEventMethod {
				continue
			}
			var ev ContextEvent
			if err := json.Unmarshal(n.Params, &ev); err != nil {
				continue
			}
			if ev.SubscriptionID != result.SubscriptionID {
				continue
			}
			select {
			case out <- ev:
			default:
			}
		}
	}()
	return result.SubscriptionID, out, nil
}

// UnsubscribeContext sends orca/context.unsubscribe.
func (t *Transport) UnsubscribeContext(ctx context.Context, subscriptionID string) error {
	params := struct {
		SubscriptionID string `json:"subscription_id"`
	}{SubscriptionID: subscriptionID}
	resp, err := t.Call(ctx, "orca/context.unsubscribe", params)
	if err != nil {
		return err
	}
	if resp.IsError() {
		return fmt.Errorf("orca/context.unsubscribe rejected: %s", resp.Error.Message)
	}
	return nil
}

// idToRaw / rawToID encode JSON-RPC ids as numbers (matching the Rust SDK).
func idToRaw(id uint64) json.RawMessage {
	return json.RawMessage(strconv.FormatUint(id, 10))
}

func rawToID(raw json.RawMessage) (uint64, bool) {
	if len(raw) == 0 {
		return 0, false
	}
	n, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}
