// orca-conformance-plugin (Kotlin) — companion to the Rust/Go/TS reference
// plugins. Exercises the canonical conformance scenario through the Kotlin
// SDK so a single host can diff observations across language ports.
package orca.sdk.bin

import java.nio.file.Path
import java.nio.file.Paths
import kotlin.system.exitProcess
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import orca.sdk.Flavor
import orca.sdk.ManifestParser
import orca.sdk.Pki
import orca.sdk.Sensitivity
import orca.sdk.ToolHandler
import orca.sdk.ToolHandlerError
import orca.sdk.Transport
import orca.sdk.TypeDeclaration
import orca.sdk.TypedValue

private const val SCENARIO_TYPE_NAME = "Greeting"
private const val SCENARIO_TYPE_SCHEMA_VERSION = "0.1.0"
private const val SCENARIO_CONTEXT_ID = "conformance:hello"
private const val SCENARIO_MANIFEST_ID_PAYLOAD_KEY = "manifest_id"

// Tools surface — must match projects/sdk/src/conformance.rs SCENARIO.
private const val SCENARIO_TOOL_NAME = "echo"
private const val SCENARIO_TOOL_ARG_KEY = "value"
private const val SCENARIO_TOOL_RESULT_ECHO_KEY = "echoed"

private val SCENARIO_TYPE_SCHEMA = Json.parseToJsonElement(
    """{"type":"object","properties":{"text":{"type":"string"},"manifest_id":{"type":"string"}},"required":["text","manifest_id"]}""",
)

private val SCENARIO_TOOL_INPUT_SCHEMA = Json.parseToJsonElement(
    """{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}""",
)

private fun envRequired(name: String): String =
    System.getenv(name) ?: error("required env var $name not set")

fun main() {
    try {
        runBlocking { run() }
    } catch (e: Exception) {
        System.err.println("orca-conformance-plugin (kotlin): ${e.message}")
        exitProcess(1)
    }
}

private suspend fun run() {
    val addr = envRequired("ORCA_PLUGIN_ADDR")
    val pkiDir: Path = Paths.get(envRequired("ORCA_PKI_DIR"))
    val pluginId = envRequired("ORCA_PLUGIN_ID")
    val manifestPath: Path = Paths.get(envRequired("ORCA_MANIFEST_PATH"))

    val mf = ManifestParser.parseFile(manifestPath)
    if (mf.plugin.id != pluginId) {
        error("manifest plugin.id '${mf.plugin.id}' != ORCA_PLUGIN_ID '$pluginId'")
    }

    val bundle = Pki.loadPlugin(pkiDir, pluginId)
    val transport = Transport.connect(addr, bundle)
    try {
        transport.hello(pluginId, Flavor.HEADLESS)

        transport.declareTypes(
            listOf(
                TypeDeclaration(
                    type_name = SCENARIO_TYPE_NAME,
                    schema_version = SCENARIO_TYPE_SCHEMA_VERSION,
                    schema = SCENARIO_TYPE_SCHEMA,
                    sensitivity = Sensitivity.GENERAL.wire,
                ),
            ),
        )

        val payload = buildJsonObject {
            put("text", "hello from the Kotlin conformance plugin")
            put(SCENARIO_MANIFEST_ID_PAYLOAD_KEY, mf.plugin.id)
        }
        transport.publishContext(
            SCENARIO_CONTEXT_ID,
            TypedValue(
                type = "${pluginId}.${SCENARIO_TYPE_NAME}",
                schema_version = SCENARIO_TYPE_SCHEMA_VERSION,
                sensitivity = Sensitivity.GENERAL.wire,
                payload = payload,
            ),
        )

        // Register echo, declare it, idle so the host's tools.call lands.
        transport.registerTool(
            name = SCENARIO_TOOL_NAME,
            description = "echo back the value argument",
            inputSchema = SCENARIO_TOOL_INPUT_SCHEMA,
            sensitivity = Sensitivity.GENERAL,
            handler = ToolHandler { args ->
                val v = args.jsonObject[SCENARIO_TOOL_ARG_KEY]?.jsonPrimitive?.content
                    ?: throw ToolHandlerError("missing '$SCENARIO_TOOL_ARG_KEY' arg")
                buildJsonObject { put(SCENARIO_TOOL_RESULT_ECHO_KEY, v) }
            },
        )
        transport.declareTools()

        delay(2_000)
    } finally {
        transport.close()
    }
}
