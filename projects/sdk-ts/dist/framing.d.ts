/**
 * Length-prefixed framing for JSON-RPC messages over a stream transport.
 *
 * Wire format (matches projects/sdk/src/framing.rs):
 *   [ 4-byte big-endian uint32 length ][ length bytes of body ]
 */
import type { Readable, Writable } from 'node:stream';
/**
 * Largest frame body either side will read or write. Mirrors the 16 MiB cap
 * in the Rust SDK; both sides MUST agree to prevent malicious or buggy peers
 * from forcing unbounded allocation.
 */
export declare const MAX_FRAME: number;
export declare class FrameTooLargeError extends Error {
    constructor(size: number);
}
/** Write one framed message: 4-byte length header followed by body. */
export declare function writeFrame(stream: Writable, body: Uint8Array): Promise<void>;
/**
 * Reader that pulls one frame at a time from a Node Readable. Internally
 * accumulates bytes and yields complete frames as Buffers.
 */
export declare class FrameReader {
    private buf;
    private waiters;
    private ended;
    private error;
    constructor(stream: Readable);
    /** Resolve to the next complete frame, or null on stream end. Rejects on error. */
    read(): Promise<Buffer | null>;
    private drain;
}
//# sourceMappingURL=framing.d.ts.map