// orca SDK — Kotlin port. Wire-compatible with projects/sdk (Rust reference),
// projects/sdk-go, and projects/sdk-ts.
//
// Targets JVM 21 so any host with a modern JDK can run the plugin. The
// conformance plugin is built as a fat ("shadow") JAR via the application
// plugin's distZip + a manual jar-with-deps Kotlin task, so the binary the
// Rust conformance suite execs is a single self-contained file.

plugins {
    kotlin("jvm") version "2.3.21"
    kotlin("plugin.serialization") version "2.3.21"
    application
}

group = "orca"
version = "0.1.0"

repositories { mavenCentral() }

kotlin {
    jvmToolchain(21)
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    implementation("org.tomlj:tomlj:1.1.1")

    testImplementation(kotlin("test"))
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2")
}

application {
    mainClass.set("orca.sdk.bin.ConformancePluginKt")
    applicationName = "orca-conformance-plugin-kt"
}

tasks.test {
    useJUnitPlatform()
}

// Build a single executable fat-jar for the conformance plugin. The Rust
// conformance test execs this file directly via a thin shell launcher.
tasks.register<Jar>("conformanceJar") {
    archiveClassifier.set("conformance-all")
    manifest {
        attributes["Main-Class"] = "orca.sdk.bin.ConformancePluginKt"
    }
    duplicatesStrategy = DuplicatesStrategy.EXCLUDE
    from(sourceSets.main.get().output)
    dependsOn(configurations.runtimeClasspath)
    from({
        configurations.runtimeClasspath.get().filter { it.name.endsWith(".jar") }.map { zipTree(it) }
    })
    exclude("META-INF/*.SF", "META-INF/*.DSA", "META-INF/*.RSA", "module-info.class")
}
