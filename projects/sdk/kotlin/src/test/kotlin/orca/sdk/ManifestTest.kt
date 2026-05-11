package orca.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

private const val CANONICAL = """
[plugin]
id               = "alpha"
version          = "0.1.0"
min_orca_version = "0.1.0"

[runtime]
binary = "./bin/alpha"
mode   = "process"
eager  = false

[surfaces]
mcp = true

[[capabilities]]
name        = "context.publish"
sensitivity = "general"

[[capabilities]]
name        = "atlassian.read"
sensitivity = "sensitive"
"""

class ManifestTest {
    @Test fun parsesCanonical() {
        val m = ManifestParser.parseString(CANONICAL)
        assertEquals("alpha", m.plugin.id)
        assertEquals("./bin/alpha", m.runtime.binary)
        assertEquals("process", m.runtime.mode)
        assertTrue(m.surfaces.mcp)
        assertEquals(2, m.capabilities.size)
        assertEquals("sensitive", m.capabilities[1].sensitivity)
    }

    @Test fun rejectsUnknownTopLevel() {
        val ex = assertFailsWith<IllegalStateException> {
            ManifestParser.parseString(CANONICAL + "\n[bogus]\nx = 1\n")
        }
        assertTrue(ex.message!!.contains("bogus"))
    }

    @Test fun rejectsBothBinaryAndImage() {
        val ex = assertFailsWith<IllegalStateException> {
            ManifestParser.parseString(
                """
                [plugin]
                id = "x"
                version = "0.1.0"
                min_orca_version = "0.1.0"

                [runtime]
                binary = "./b"
                image  = "ghcr.io/x:1"
                """.trimIndent(),
            )
        }
        assertTrue(ex.message!!.contains("mutually exclusive"))
    }

    @Test fun rejectsDuplicateCapability() {
        val ex = assertFailsWith<IllegalStateException> {
            ManifestParser.parseString(
                """
                [plugin]
                id = "x"
                version = "0.1.0"
                min_orca_version = "0.1.0"

                [runtime]
                binary = "./b"

                [[capabilities]]
                name = "thing"
                sensitivity = "general"

                [[capabilities]]
                name = "thing"
                sensitivity = "sensitive"
                """.trimIndent(),
            )
        }
        assertTrue(ex.message!!.contains("duplicate"))
    }

    @Test fun rejectsPreReleaseSemver() {
        val ex = assertFailsWith<IllegalStateException> {
            ManifestParser.parseString(
                """
                [plugin]
                id = "x"
                version = "0.1.0-rc1"
                min_orca_version = "0.1.0"

                [runtime]
                binary = "./b"
                """.trimIndent(),
            )
        }
        assertTrue(ex.message!!.contains("pre-release"))
    }
}
