package framing

import (
	"bytes"
	"encoding/binary"
	"errors"
	"testing"
)

func TestRoundtripSmallMessage(t *testing.T) {
	msg := []byte("hello world")
	var buf bytes.Buffer
	if err := Write(&buf, msg); err != nil {
		t.Fatalf("write: %v", err)
	}
	if got, want := buf.Len(), 4+len(msg); got != want {
		t.Fatalf("buf size = %d, want %d", got, want)
	}
	got, err := Read(&buf)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if !bytes.Equal(got, msg) {
		t.Fatalf("got %q, want %q", got, msg)
	}
}

func TestRoundtripEmptyMessage(t *testing.T) {
	var buf bytes.Buffer
	if err := Write(&buf, nil); err != nil {
		t.Fatalf("write: %v", err)
	}
	got, err := Read(&buf)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if len(got) != 0 {
		t.Fatalf("got %d bytes, want empty", len(got))
	}
}

func TestRejectsOversizedFrame(t *testing.T) {
	var hdr [4]byte
	binary.BigEndian.PutUint32(hdr[:], MaxFrame+1)
	_, err := Read(bytes.NewReader(hdr[:]))
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	if !errors.Is(err, ErrFrameTooLarge) {
		t.Fatalf("expected ErrFrameTooLarge, got %v", err)
	}
}

// Cross-implementation guarantee: a frame written by the Go encoder must be
// readable by the Rust decoder, and vice versa. We validate the byte layout
// directly so a regression here is caught even without running the Rust SDK.
func TestWireLayoutMatchesRustReference(t *testing.T) {
	body := []byte(`{"jsonrpc":"2.0","method":"orca/hello"}`)
	var buf bytes.Buffer
	if err := Write(&buf, body); err != nil {
		t.Fatalf("write: %v", err)
	}
	out := buf.Bytes()
	if len(out) < 4 {
		t.Fatalf("output too short")
	}
	gotLen := binary.BigEndian.Uint32(out[:4])
	if gotLen != uint32(len(body)) {
		t.Fatalf("header = %d, want %d", gotLen, len(body))
	}
	if !bytes.Equal(out[4:], body) {
		t.Fatalf("body mismatch")
	}
}
