// Length-prefixed framing for JSON-RPC messages over a stream transport.
// Wire format (matches projects/sdk/src/framing.rs):
//   [ 4-byte big-endian uint32 length ][ length bytes of body ]
package orca.sdk

import java.io.DataInputStream
import java.io.OutputStream

/**
 * Largest frame body either side will read or write. Mirrors the 16 MiB cap
 * in the Rust SDK; both sides MUST agree to prevent malicious or buggy peers
 * from forcing unbounded allocation.
 */
const val MAX_FRAME: Int = 16 * 1024 * 1024

class FrameTooLargeException(size: Long) : RuntimeException(
    "frame too large: $size bytes (max $MAX_FRAME)"
)

object Framing {
    /** Write one framed message: 4-byte BE length followed by body. */
    fun write(out: OutputStream, body: ByteArray) {
        if (body.size.toLong() > MAX_FRAME.toLong()) throw FrameTooLargeException(body.size.toLong())
        val header = ByteArray(4)
        header[0] = ((body.size ushr 24) and 0xff).toByte()
        header[1] = ((body.size ushr 16) and 0xff).toByte()
        header[2] = ((body.size ushr 8) and 0xff).toByte()
        header[3] = (body.size and 0xff).toByte()
        out.write(header)
        if (body.isNotEmpty()) out.write(body)
        out.flush()
    }

    /** Read one framed message. Returns null on EOF before any header byte. */
    fun read(input: DataInputStream): ByteArray? {
        val header = ByteArray(4)
        var got = 0
        while (got < 4) {
            val n = input.read(header, got, 4 - got)
            if (n <= 0) {
                if (got == 0) return null
                throw java.io.EOFException("truncated frame header")
            }
            got += n
        }
        val size =
            ((header[0].toInt() and 0xff) shl 24) or
                ((header[1].toInt() and 0xff) shl 16) or
                ((header[2].toInt() and 0xff) shl 8) or
                (header[3].toInt() and 0xff)
        val sizeUnsigned = size.toLong() and 0xffffffffL
        if (sizeUnsigned > MAX_FRAME) throw FrameTooLargeException(sizeUnsigned)
        if (size == 0) return ByteArray(0)
        val body = ByteArray(size)
        input.readFully(body)
        return body
    }
}
