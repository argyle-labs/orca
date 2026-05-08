// TCP + mTLS transport: plugin (client) side. Wire-compatible with
// projects/sdk/src/transport.rs, projects/sdk-go/transport, and
// projects/sdk-ts/src/transport.ts.
package orca.sdk

import java.io.DataInputStream
import java.net.InetSocketAddress
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong
import javax.net.ssl.SSLSocket
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.consumeAsFlow
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import kotlinx.coroutines.runInterruptible
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.long
import kotlinx.serialization.json.put

const val SDK_VERSION: String = "0.1.0"
const val CONTEXT_EVENT_METHOD: String = "orca/context.event"

enum class Flavor(val wire: String) {
    FULL("full"),
    HEADLESS("headless"),
    LOCAL("local");
}

enum class Sensitivity(val wire: String) {
    GENERAL("general"),
    SENSITIVE("sensitive");
}

@Serializable
data class HelloResult(
    val server_version: String,
    val ok: Boolean,
    val status: String,
    val methods: List<String>,
    val reason: String? = null,
)

@Serializable
data class TypesDeclareResult(val accepted: List<String>)

@Serializable
data class TypedValue(
    val type: String,
    val schema_version: String,
    val sensitivity: String,
    val payload: JsonElement,
)

@Serializable
data class ContextEvent(
    val subscription_id: String,
    val context_id: String,
    val value: TypedValue,
)

private val json = Json {
    encodeDefaults = true
    ignoreUnknownKeys = true
    explicitNulls = false
}

class Transport private constructor(
    private val socket: SSLSocket,
    private val parentJob: Job,
) {
    private val scope = CoroutineScope(SupervisorJob(parentJob) + Dispatchers.IO)
    private val output = socket.outputStream
    private val input = DataInputStream(socket.inputStream)
    private val nextId = AtomicLong(1)
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<Response>>()
    private val notifFlow = MutableSharedFlow<Notification>(extraBufferCapacity = 256)
    private val writeMutex = Object()

    init {
        scope.launch { readLoop() }
    }

    companion object {
        /**
         * Connect to host:port over mTLS using `bundle`. Suspends until the
         * TLS handshake completes.
         */
        suspend fun connect(addr: String, bundle: NodeBundle): Transport {
            val (host, portStr) = addr.split(":", limit = 2).let {
                require(it.size == 2) { "invalid addr: $addr" }
                it[0] to it[1]
            }
            val sslContext = Pki.clientSslContext(bundle)
            val factory = sslContext.socketFactory
            return runInterruptible(Dispatchers.IO) {
                val raw = java.net.Socket()
                raw.connect(InetSocketAddress(host, portStr.toInt()), 10_000)
                val ssl = factory.createSocket(raw, "core.orca.local", portStr.toInt(), true) as SSLSocket
                ssl.useClientMode = true
                ssl.startHandshake()
                Transport(ssl, Job())
            }
        }
    }

    fun close() {
        try {
            socket.close()
        } catch (_: Exception) {}
        scope.cancel()
        for ((_, def) in pending) {
            def.complete(
                Response(
                    id = null,
                    error = ErrorObject(CODE_INTERNAL_ERROR, "transport closed"),
                )
            )
        }
        pending.clear()
    }

    /** Flow of every server-pushed notification. */
    fun notifications(): Flow<Notification> = notifFlow.asSharedFlow()

    private suspend fun readLoop() {
        try {
            while (true) {
                val frame = runInterruptible { Framing.read(input) } ?: break
                val raw = json.parseToJsonElement(String(frame, Charsets.UTF_8)).jsonObject
                val hasMethod = raw.containsKey("method")
                val hasId = raw.containsKey("id")
                val hasResultOrError = raw.containsKey("result") || raw.containsKey("error")
                when {
                    hasId && hasResultOrError && !hasMethod -> {
                        val resp = json.decodeFromJsonElement(Response.serializer(), raw)
                        val id = resp.id
                        if (id != null) pending.remove(id)?.complete(resp)
                    }
                    hasMethod && !hasId -> {
                        val notif = json.decodeFromJsonElement(Notification.serializer(), raw)
                        notifFlow.tryEmit(notif)
                    }
                    // Server-to-plugin requests not part of Phase A.
                }
            }
        } catch (_: Exception) {
            // Stream closed or malformed — fall through to close.
        } finally {
            close()
        }
    }

    private fun writeBytes(body: ByteArray) {
        synchronized(writeMutex) { Framing.write(output, body) }
    }

    /** Send a request and suspend until the matching response arrives. */
    suspend fun call(method: String, params: JsonElement?): Response {
        val id = nextId.getAndIncrement()
        val def = CompletableDeferred<Response>()
        pending[id] = def
        val req = buildJsonObject {
            put("jsonrpc", JSONRPC_VERSION)
            put("id", id)
            put("method", method)
            if (params != null) put("params", params)
        }
        try {
            writeBytes(json.encodeToString(JsonObject.serializer(), req).toByteArray())
        } catch (e: Exception) {
            pending.remove(id)
            throw e
        }
        return def.await()
    }

    suspend fun hello(
        pluginId: String,
        flavor: Flavor = Flavor.HEADLESS,
        methodsRequired: List<String> = emptyList(),
        methodsOptional: List<String> = emptyList(),
    ): HelloResult {
        val params = buildJsonObject {
            put("sdk_version", SDK_VERSION)
            put("plugin_id", pluginId)
            put("flavor", flavor.wire)
            put("core_min_required", "0.1.0")
            put("methods_required", json.encodeToJsonElement(methodsRequired))
            put("methods_optional", json.encodeToJsonElement(methodsOptional))
        }
        val resp = call("orca/hello", params)
        if (resp.isError) error("orca/hello rejected: ${resp.error?.message}")
        val result = json.decodeFromJsonElement(HelloResult.serializer(), resp.result!!)
        if (!result.ok) error("orca/hello: server returned ok=false (status=${result.status}; ${result.reason ?: "no reason"})")
        return result
    }

    suspend fun declareTypes(types: List<TypeDeclaration>): TypesDeclareResult {
        val params = buildJsonObject {
            put("types", json.encodeToJsonElement(types))
        }
        val resp = call("orca/types.declare", params)
        if (resp.isError) error("orca/types.declare rejected: ${resp.error?.message}")
        return json.decodeFromJsonElement(TypesDeclareResult.serializer(), resp.result!!)
    }

    suspend fun publishContext(contextId: String, value: TypedValue) {
        val params = buildJsonObject {
            put("context_id", contextId)
            put("value", json.encodeToJsonElement(value))
        }
        val resp = call("orca/context.publish", params)
        if (resp.isError) error("orca/context.publish rejected: ${resp.error?.message}")
    }

    suspend fun subscribeContext(
        contextId: String,
        typeFilter: List<String> = emptyList(),
    ): Pair<String, Flow<ContextEvent>> {
        val params = buildJsonObject {
            put("context_id", contextId)
            put("type_filter", json.encodeToJsonElement(typeFilter))
        }
        val resp = call("orca/context.subscribe", params)
        if (resp.isError) error("orca/context.subscribe rejected: ${resp.error?.message}")
        val subId = resp.result!!.jsonObject["subscription_id"]!!.jsonPrimitive.toString().trim('"')

        val events = Channel<ContextEvent>(Channel.BUFFERED)
        scope.launch {
            notifFlow.filter { it.method == CONTEXT_EVENT_METHOD }.collect { n ->
                val ev = json.decodeFromJsonElement(ContextEvent.serializer(), n.params!!)
                if (ev.subscription_id == subId) events.send(ev)
            }
        }
        return subId to events.consumeAsFlow()
    }

    suspend fun unsubscribeContext(subscriptionId: String) {
        val params = buildJsonObject { put("subscription_id", subscriptionId) }
        val resp = call("orca/context.unsubscribe", params)
        if (resp.isError) error("orca/context.unsubscribe rejected: ${resp.error?.message}")
    }
}

@Serializable
data class TypeDeclaration(
    val type_name: String,
    val schema_version: String,
    val schema: JsonElement,
    val sensitivity: String,
)

private inline fun <reified T> Json.encodeToJsonElement(value: T): JsonElement =
    parseToJsonElement(encodeToString(value))

/** Convenience: load a bundle from `pkiDir` and connect. */
suspend fun connectAsPlugin(addr: String, pkiDir: Path, pluginId: String): Transport {
    val bundle = Pki.loadPlugin(pkiDir, pluginId)
    return Transport.connect(addr, bundle)
}
