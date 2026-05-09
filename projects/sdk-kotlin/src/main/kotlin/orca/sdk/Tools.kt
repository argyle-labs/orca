// Tools surface — plugins declare callable tools, host invokes them.
// Wire-compatible with projects/sdk/src/tools.rs, projects/sdk-go/tools,
// and projects/sdk-ts/src/tools.ts.
//
// Plugins declare bare names like "stack.list"; the host owns the
// namespace and registers them as "<plugin_id>.stack.list".
package orca.sdk

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement

/** Wire method names. */
object ToolsProtocol {
    const val DECLARE_METHOD = "orca/tools.declare"
    const val CALL_METHOD = "orca/tools.call"

    /**
     * Plugin → host cross-plugin invocation. Caller supplies a
     * fully-qualified tool name `<plugin>.<tool>`; host resolves the
     * owning peer via the in-process registry and forwards tools.call.
     */
    const val INVOKE_METHOD = "orca/tools.invoke"

    /**
     * Plugin → host peer enumeration. Returns connected peers + versions.
     */
    const val PLUGINS_LIST_METHOD = "orca/plugins.list"
}

/**
 * JSON-RPC error codes specific to the tools surface. Extends the
 * standard -32600..-32099 range. Match projects/sdk/src/tools.rs.
 */
object ToolErrorCodes {
    /** The named tool is not registered for this plugin. */
    const val UNKNOWN_TOOL = -32001

    /** Arguments did not match the declared input_schema. */
    const val SCHEMA_VIOLATION = -32002

    /** Handler ran but returned an application error. */
    const val HANDLER_ERROR = -32003
}

/**
 * One tool the plugin announces. Fully-qualified id is `<plugin_id>.<name>`,
 * computed host-side.
 */
@Serializable
data class ToolDeclaration(
    val name: String,
    val description: String,
    val input_schema: JsonElement,
    val sensitivity: String,
)

/** Wire shape of orca/tools.declare params. */
@Serializable
data class ToolsDeclareParams(val tools: List<ToolDeclaration>)

/** Wire shape of orca/tools.declare result — namespaced ids the host accepted. */
@Serializable
data class ToolsDeclareResult(val accepted: List<String>)

/**
 * Wire shape of orca/tools.call params. `name` is the bare tool name (no
 * `<plugin_id>.` prefix — the host strips it before dispatch).
 */
@Serializable
data class ToolCallParams(val name: String, val arguments: JsonElement)

/** Wire shape of orca/tools.call result — opaque JSON. */
@Serializable
data class ToolCallResult(val result: JsonElement)

/** Wire shape of orca/tools.invoke params. `name` is fq `<peer>.<tool>`. */
@Serializable
data class ToolInvokeParams(
    val name: String,
    val arguments: JsonElement,
    val timeout_secs: Long? = null,
)

/** Wire shape of orca/tools.invoke result — peer's opaque tool result. */
@Serializable
data class ToolInvokeResult(val result: JsonElement)

/** One entry in orca/plugins.list — a connected peer and its version. */
@Serializable
data class PeerInfo(val id: String, val version: String)

/** Wire shape of orca/plugins.list result. */
@Serializable
data class PluginsListResult(val peers: List<PeerInfo>)

/**
 * Application-level error a tool handler can throw. The transport
 * translates this into a JSON-RPC error response with code
 * [ToolErrorCodes.HANDLER_ERROR]. Any other exception is treated as
 * internal.
 */
class ToolHandlerError(message: String, val data: JsonElement? = null) : Exception(message)

/**
 * Functional interface every tool implementation satisfies. SAM means
 * callers can pass a plain lambda to [Transport.registerTool].
 */
fun interface ToolHandler {
    suspend fun call(args: JsonElement): JsonElement
}

/**
 * Bundles a declaration with its handler. Stored in the transport's
 * per-connection registry; not part of the wire format.
 */
data class RegisteredTool(val declaration: ToolDeclaration, val handler: ToolHandler)
