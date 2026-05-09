package orca.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertTrue
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put

class ToolsTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test
    fun declarationRoundtrips() {
        val d = ToolDeclaration(
            name = "stack.list",
            description = "List stacks",
            input_schema = Json.parseToJsonElement("""{"type":"object"}"""),
            sensitivity = "general",
        )
        val s = json.encodeToString(ToolDeclaration.serializer(), d)
        val back = json.decodeFromString(ToolDeclaration.serializer(), s)
        assertEquals("stack.list", back.name)
        assertEquals("general", back.sensitivity)
    }

    @Test
    fun toolHandlerErrorCarriesData() {
        val data = buildJsonObject { put("status", 500) }
        val e = ToolHandlerError("upstream rejected", data)
        assertEquals("upstream rejected", e.message)
        assertNotNull(e.data)
        assertEquals(500, e.data!!.jsonObject["status"]!!.jsonPrimitive.content.toInt())
    }

    @Test
    fun handlerSamAcceptsLambda() = runBlocking {
        val h: ToolHandler = ToolHandler { args ->
            val v = args.jsonObject["value"]!!.jsonPrimitive.content
            buildJsonObject { put("echoed", v) }
        }
        val out = h.call(buildJsonObject { put("value", "ping") })
        assertEquals("ping", out.jsonObject["echoed"]!!.jsonPrimitive.content)
    }

    @Test
    fun protocolConstantsMatchTheLockedContract() {
        assertEquals("orca/tools.declare", ToolsProtocol.DECLARE_METHOD)
        assertEquals("orca/tools.call", ToolsProtocol.CALL_METHOD)
        assertEquals(-32001, ToolErrorCodes.UNKNOWN_TOOL)
        assertEquals(-32002, ToolErrorCodes.SCHEMA_VIOLATION)
        assertEquals(-32003, ToolErrorCodes.HANDLER_ERROR)
    }

    @Test
    fun resultPayloadShape() {
        val r = ToolCallResult(buildJsonObject { put("echoed", "ping") })
        val s = json.encodeToString(ToolCallResult.serializer(), r)
        assertTrue(s.contains("\"result\""))
        assertTrue(s.contains("\"echoed\""))
    }
}
