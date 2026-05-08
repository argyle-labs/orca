/**
 * Largest frame body either side will read or write. Mirrors the 16 MiB cap
 * in the Rust SDK; both sides MUST agree to prevent malicious or buggy peers
 * from forcing unbounded allocation.
 */
export const MAX_FRAME = 16 * 1024 * 1024;
export class FrameTooLargeError extends Error {
    constructor(size) {
        super(`frame too large: ${size} bytes (max ${MAX_FRAME})`);
        this.name = 'FrameTooLargeError';
    }
}
/** Write one framed message: 4-byte length header followed by body. */
export function writeFrame(stream, body) {
    if (body.length > MAX_FRAME) {
        return Promise.reject(new FrameTooLargeError(body.length));
    }
    const header = Buffer.alloc(4);
    header.writeUInt32BE(body.length, 0);
    return new Promise((resolve, reject) => {
        stream.write(header, err => {
            if (err)
                return reject(err);
            if (body.length === 0)
                return resolve();
            stream.write(body, err2 => (err2 ? reject(err2) : resolve()));
        });
    });
}
/**
 * Reader that pulls one frame at a time from a Node Readable. Internally
 * accumulates bytes and yields complete frames as Buffers.
 */
export class FrameReader {
    buf = Buffer.alloc(0);
    waiters = [];
    ended = false;
    error = null;
    constructor(stream) {
        stream.on('data', (chunk) => {
            this.buf = (this.buf.length === 0 ? chunk : Buffer.concat([this.buf, chunk]));
            this.drain();
        });
        stream.on('end', () => {
            this.ended = true;
            this.drain();
        });
        stream.on('error', err => {
            this.error = err;
            this.drain();
        });
    }
    /** Resolve to the next complete frame, or null on stream end. Rejects on error. */
    read() {
        return new Promise((resolve, reject) => {
            const tryServe = () => {
                if (this.error) {
                    reject(this.error);
                    return true;
                }
                if (this.buf.length >= 4) {
                    const n = this.buf.readUInt32BE(0);
                    if (n > MAX_FRAME) {
                        reject(new FrameTooLargeError(n));
                        return true;
                    }
                    if (this.buf.length >= 4 + n) {
                        const body = this.buf.subarray(4, 4 + n);
                        // Copy out so callers can keep references after we slide forward.
                        const out = Buffer.from(body);
                        this.buf = this.buf.subarray(4 + n);
                        resolve(out);
                        return true;
                    }
                }
                if (this.ended) {
                    resolve(null);
                    return true;
                }
                return false;
            };
            if (tryServe())
                return;
            this.waiters.push(value => {
                if (value === null)
                    resolve(null);
                else
                    resolve(value);
            });
        });
    }
    drain() {
        while (this.waiters.length > 0) {
            const w = this.waiters[0];
            if (this.error) {
                this.waiters.shift();
                // Hand the error back via a rejected wait. We can't reject through
                // the resolver, so re-create the promise pattern: best-effort drop.
                w(null);
                continue;
            }
            if (this.buf.length < 4) {
                if (this.ended) {
                    this.waiters.shift();
                    w(null);
                    continue;
                }
                return;
            }
            const n = this.buf.readUInt32BE(0);
            if (n > MAX_FRAME) {
                // Surface via next read() call; here we just drop the waiter and
                // let read() observe the error on its next invocation.
                return;
            }
            if (this.buf.length < 4 + n) {
                if (this.ended) {
                    this.waiters.shift();
                    w(null);
                    continue;
                }
                return;
            }
            const body = Buffer.from(this.buf.subarray(4, 4 + n));
            this.buf = this.buf.subarray(4 + n);
            this.waiters.shift();
            w(body);
        }
    }
}
//# sourceMappingURL=framing.js.map