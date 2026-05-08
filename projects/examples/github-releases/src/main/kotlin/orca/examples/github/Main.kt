// github-releases — orca plugin example.
//
// Polls the public GitHub Releases API for the repos listed in
// GITHUB_REPOS (comma-separated owner/repo) and publishes each release as
// a `Release` TypedValue into the `dev:releases` context. Already-seen
// release ids are remembered in-memory so a long-running plugin only emits
// new ones after the first poll.
//
// Required env (in addition to the four ORCA_* the host injects):
//
//   GITHUB_REPOS    e.g. "anthropics/claude-code,rust-lang/rust"
//   GITHUB_TOKEN    Optional. Bumps rate limit from 60/hr to 5000/hr.
//
// Standalone dev:
//
//   ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
//   ORCA_PKI_DIR=$HOME/.orca/pki \
//   ORCA_PLUGIN_ID=github-releases \
//   GITHUB_REPOS=rust-lang/rust \
//   gradle run
//
// API: https://docs.github.com/en/rest/releases/releases
package orca.examples.github

import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.nio.file.Path
import java.nio.file.Paths
import java.time.Duration as JDuration
import java.util.concurrent.ConcurrentHashMap
import kotlin.system.exitProcess
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import orca.sdk.Flavor
import orca.sdk.Pki
import orca.sdk.Sensitivity
import orca.sdk.Transport
import orca.sdk.TypeDeclaration
import orca.sdk.TypedValue

private const val TYPE_NAME = "Release"
private const val SCHEMA_VERSION = "0.1.0"
private const val CONTEXT_ID = "dev:releases"
private const val POLL_INTERVAL_MS = 5L * 60 * 1000

private val SCHEMA = Json.parseToJsonElement(
    """{
        "type": "object",
        "properties": {
          "repo": { "type": "string" },
          "id": { "type": "integer" },
          "tag_name": { "type": "string" },
          "name": { "type": "string" },
          "html_url": { "type": "string" },
          "draft": { "type": "boolean" },
          "prerelease": { "type": "boolean" },
          "published_at": { "type": "string" }
        },
        "required": ["repo", "id", "tag_name"]
    }""",
)

@Serializable
private data class GhRelease(
    val id: Long,
    val tag_name: String,
    val name: String? = null,
    val html_url: String,
    val draft: Boolean = false,
    val prerelease: Boolean = false,
    val published_at: String? = null,
)

private fun envRequired(name: String): String =
    System.getenv(name) ?: error("required env var $name not set")

private val json = Json { ignoreUnknownKeys = true }

private fun fetchReleases(http: HttpClient, repo: String, token: String?): List<GhRelease> {
    val uri = URI("https://api.github.com/repos/$repo/releases?per_page=10")
    val builder = HttpRequest.newBuilder(uri)
        .timeout(JDuration.ofSeconds(15))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "orca-example-github-releases/0.1.0")
    if (!token.isNullOrEmpty()) builder.header("Authorization", "Bearer $token")
    val resp = http.send(builder.GET().build(), HttpResponse.BodyHandlers.ofString())
    if (resp.statusCode() != 200) {
        error("github $repo: ${resp.statusCode()} ${resp.body().take(200)}")
    }
    return json.decodeFromString(kotlinx.serialization.builtins.ListSerializer(GhRelease.serializer()), resp.body())
}

fun main(): Unit = runBlocking {
    try {
        val addr = envRequired("ORCA_PLUGIN_ADDR")
        val pkiDir: Path = Paths.get(envRequired("ORCA_PKI_DIR"))
        val pluginId = envRequired("ORCA_PLUGIN_ID")
        val repos = envRequired("GITHUB_REPOS").split(",").map { it.trim() }.filter { it.isNotEmpty() }
        val token = System.getenv("GITHUB_TOKEN")

        val bundle = Pki.loadPlugin(pkiDir, pluginId)
        val transport = Transport.connect(addr, bundle)
        try {
            transport.hello(pluginId, Flavor.HEADLESS)
            transport.declareTypes(
                listOf(
                    TypeDeclaration(
                        type_name = TYPE_NAME,
                        schema_version = SCHEMA_VERSION,
                        schema = SCHEMA,
                        sensitivity = Sensitivity.GENERAL.wire,
                    ),
                ),
            )
            val typeId = "$pluginId.$TYPE_NAME"
            val seen = ConcurrentHashMap<String, MutableSet<Long>>()
            val http = HttpClient.newBuilder().connectTimeout(JDuration.ofSeconds(10)).build()

            while (true) {
                for (repo in repos) {
                    val releases = try {
                        fetchReleases(http, repo, token)
                    } catch (e: Exception) {
                        System.err.println("github fetch $repo: ${e.message}")
                        continue
                    }
                    val seenForRepo = seen.computeIfAbsent(repo) { HashSet() }
                    val firstSeen = seenForRepo.isEmpty()
                    for (rel in releases) {
                        if (!seenForRepo.add(rel.id)) continue
                        // On first poll just record state; emit only what shows up later.
                        if (firstSeen) continue
                        val payload = buildJsonObject {
                            put("repo", repo)
                            put("id", rel.id)
                            put("tag_name", rel.tag_name)
                            put("name", rel.name ?: "")
                            put("html_url", rel.html_url)
                            put("draft", rel.draft)
                            put("prerelease", rel.prerelease)
                            put("published_at", rel.published_at ?: "")
                        }
                        transport.publishContext(
                            CONTEXT_ID,
                            TypedValue(
                                type = typeId,
                                schema_version = SCHEMA_VERSION,
                                sensitivity = Sensitivity.GENERAL.wire,
                                payload = payload,
                            ),
                        )
                    }
                }
                delay(POLL_INTERVAL_MS)
            }
        } finally {
            transport.close()
        }
    } catch (e: Exception) {
        System.err.println("orca-example-github-releases: ${e.message}")
        exitProcess(1)
    }
}
