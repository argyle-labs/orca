// Plugin manifest — `orca-plugin.toml`. Wire-compatible with
// projects/sdk/src/manifest.rs and the other language ports.
package orca.sdk

import java.nio.file.Files
import java.nio.file.Path
import org.tomlj.Toml
import org.tomlj.TomlParseResult
import org.tomlj.TomlTable

const val MANIFEST_FILENAME: String = "orca-plugin.toml"

data class PluginSection(
    val id: String,
    val version: String,
    val minOrcaVersion: String,
)

data class RuntimeSection(
    val binary: String?,
    val image: String?,
    val mode: String,
    val eager: Boolean,
)

data class SurfacesSection(
    val mcp: Boolean = false,
    val cli: Boolean = false,
    val ui: Boolean = false,
    val docs: Boolean = false,
    val jobs: Boolean = false,
    val storage: Boolean = false,
    val federation: Boolean = false,
)

data class CapabilityDecl(val name: String, val sensitivity: String)

data class Manifest(
    val plugin: PluginSection,
    val runtime: RuntimeSection,
    val surfaces: SurfacesSection,
    val capabilities: List<CapabilityDecl>,
)

private val ALLOWED_TOP = setOf("plugin", "runtime", "surfaces", "capabilities")
private val ALLOWED_PLUGIN = setOf("id", "version", "min_orca_version")
private val ALLOWED_RUNTIME = setOf("binary", "image", "mode", "eager")
private val ALLOWED_SURFACES = setOf("mcp", "cli", "ui", "docs", "jobs", "storage", "federation")
private val ALLOWED_CAPABILITY = setOf("name", "sensitivity")

object ManifestParser {
    fun parseString(s: String): Manifest {
        val parsed: TomlParseResult = Toml.parse(s)
        if (parsed.hasErrors()) {
            error("parse $MANIFEST_FILENAME: ${parsed.errors().joinToString("; ")}")
        }
        denyUnknown(parsed.keySet(), ALLOWED_TOP, "top-level")

        val pluginTable = parsed.getTable("plugin") ?: error("missing [plugin]")
        denyUnknown(pluginTable.keySet(), ALLOWED_PLUGIN, "[plugin]")
        val id = (pluginTable.getString("id") ?: "").trim()
        if (id.isEmpty()) error("plugin.id must not be empty")
        for (ch in id) {
            if (ch == ' ' || ch == '\t' || ch == '\n' || ch == '/' || ch == '\\') {
                error("plugin.id \"$id\" contains invalid characters (whitespace or path separators)")
            }
        }
        val version = pluginTable.getString("version") ?: error("plugin.version missing")
        val minOrca = pluginTable.getString("min_orca_version") ?: error("plugin.min_orca_version missing")
        checkSemver(version, "plugin.version")
        checkSemver(minOrca, "plugin.min_orca_version")

        val runtimeTable = parsed.getTable("runtime") ?: error("missing [runtime]")
        denyUnknown(runtimeTable.keySet(), ALLOWED_RUNTIME, "[runtime]")
        val binary = runtimeTable.getString("binary")
        val image = runtimeTable.getString("image")
        if (binary != null && image != null) error("runtime.binary and runtime.image are mutually exclusive")
        if (binary == null && image == null) error("runtime requires either `binary` or `image`")
        val mode = runtimeTable.getString("mode") ?: "process"
        if (mode != "process") error("runtime.mode \"$mode\": only \"process\" is supported in v0")
        val eager = runtimeTable.getBoolean("eager") ?: false

        val surfacesTable = parsed.getTable("surfaces")
        if (surfacesTable != null) denyUnknown(surfacesTable.keySet(), ALLOWED_SURFACES, "[surfaces]")
        val surfaces = SurfacesSection(
            mcp = surfacesTable?.getBoolean("mcp") ?: false,
            cli = surfacesTable?.getBoolean("cli") ?: false,
            ui = surfacesTable?.getBoolean("ui") ?: false,
            docs = surfacesTable?.getBoolean("docs") ?: false,
            jobs = surfacesTable?.getBoolean("jobs") ?: false,
            storage = surfacesTable?.getBoolean("storage") ?: false,
            federation = surfacesTable?.getBoolean("federation") ?: false,
        )

        val capsArray = parsed.getArray("capabilities")
        val seen = HashSet<String>()
        val capabilities = buildList {
            if (capsArray != null) {
                for (i in 0 until capsArray.size()) {
                    val tbl = capsArray.getTable(i)
                    denyUnknown(tbl.keySet(), ALLOWED_CAPABILITY, "[[capabilities]]")
                    val name = (tbl.getString("name") ?: "").trim()
                    if (name.isEmpty()) error("capability.name must not be empty")
                    if (!seen.add(name)) error("duplicate capability \"$name\"")
                    val sensitivity = tbl.getString("sensitivity")
                        ?: error("capability \"$name\": sensitivity missing")
                    if (sensitivity != "general" && sensitivity != "sensitive") {
                        error("capability \"$name\": sensitivity must be \"general\" or \"sensitive\", got \"$sensitivity\"")
                    }
                    add(CapabilityDecl(name, sensitivity))
                }
            }
        }

        return Manifest(
            plugin = PluginSection(id, version, minOrca),
            runtime = RuntimeSection(binary, image, mode, eager),
            surfaces = surfaces,
            capabilities = capabilities,
        )
    }

    fun parseFile(path: Path): Manifest = parseString(Files.readString(path))

    private fun denyUnknown(keys: Set<String>, allowed: Set<String>, where: String) {
        for (k in keys) {
            if (k !in allowed) error("unknown field \"$k\" in $where")
        }
    }

    private fun checkSemver(v: String, field: String) {
        if (v.contains('-') || v.contains('+')) {
            error("$field \"$v\": pre-release/build metadata not supported in v0")
        }
        val parts = v.split('.')
        if (parts.isEmpty()) error("$field \"$v\": empty")
        for (p in parts) {
            if (p.isEmpty()) error("$field \"$v\": empty component")
            if (p.toULongOrNull() == null) error("$field \"$v\": bad numeric component \"$p\"")
        }
    }
}
