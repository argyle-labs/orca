// JSON-RPC 2.0 wire types. Wire-compatible with projects/sdk/src/jsonrpc.rs,
// projects/sdk-go/jsonrpc, and projects/sdk-ts.
package orca.sdk

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement

const val JSONRPC_VERSION: String = "2.0"

@Serializable
data class Request(
    val jsonrpc: String = JSONRPC_VERSION,
    val id: Long,
    val method: String,
    val params: JsonElement? = null,
)

@Serializable
data class Notification(
    val jsonrpc: String = JSONRPC_VERSION,
    val method: String,
    val params: JsonElement? = null,
)

@Serializable
data class ErrorObject(
    val code: Int,
    val message: String,
    val data: JsonElement? = null,
)

@Serializable
data class Response(
    val jsonrpc: String = JSONRPC_VERSION,
    val id: Long? = null,
    val result: JsonElement? = null,
    val error: ErrorObject? = null,
) {
    val isError: Boolean get() = error != null
}

const val CODE_METHOD_NOT_FOUND: Int = -32601
const val CODE_INVALID_PARAMS: Int = -32602
const val CODE_INTERNAL_ERROR: Int = -32603
