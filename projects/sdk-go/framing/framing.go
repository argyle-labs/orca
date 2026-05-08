// Package framing implements length-prefixed framing for JSON-RPC messages
// over a stream transport.
//
// Wire format (matches projects/sdk/src/framing.rs):
//
//	[ 4-byte big-endian uint32 length ][ length bytes of body ]
package framing

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

// MaxFrame is the largest frame body we will read or write. Mirrors the
// 16 MiB cap in the Rust SDK; both sides MUST agree to prevent malicious or
// buggy peers from forcing unbounded allocation.
const MaxFrame uint32 = 16 * 1024 * 1024

// ErrFrameTooLarge is returned when a peer announces a frame body bigger
// than MaxFrame.
var ErrFrameTooLarge = errors.New("frame too large")

// Write emits one framed message: 4-byte length header followed by body.
// Returns an error if body is too large to fit in a uint32 length, or if
// the underlying writer fails.
func Write(w io.Writer, body []byte) error {
	if uint64(len(body)) > uint64(MaxFrame) {
		return fmt.Errorf("message too large to frame: %d bytes (max %d)", len(body), MaxFrame)
	}
	var hdr [4]byte
	binary.BigEndian.PutUint32(hdr[:], uint32(len(body)))
	if _, err := w.Write(hdr[:]); err != nil {
		return fmt.Errorf("write frame header: %w", err)
	}
	if len(body) == 0 {
		return nil
	}
	if _, err := w.Write(body); err != nil {
		return fmt.Errorf("write frame body: %w", err)
	}
	return nil
}

// Read consumes one framed message from r: a 4-byte big-endian length, then
// exactly that many bytes. Returns ErrFrameTooLarge if the announced length
// exceeds MaxFrame.
func Read(r io.Reader) ([]byte, error) {
	var hdr [4]byte
	if _, err := io.ReadFull(r, hdr[:]); err != nil {
		return nil, fmt.Errorf("read frame header: %w", err)
	}
	n := binary.BigEndian.Uint32(hdr[:])
	if n > MaxFrame {
		return nil, fmt.Errorf("%w: %d bytes (max %d)", ErrFrameTooLarge, n, MaxFrame)
	}
	if n == 0 {
		return []byte{}, nil
	}
	body := make([]byte, n)
	if _, err := io.ReadFull(r, body); err != nil {
		return nil, fmt.Errorf("read frame body: %w", err)
	}
	return body, nil
}
