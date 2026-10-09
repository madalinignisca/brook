// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import javax.inject.Inject

plugins {
    id("com.android.application")
    // AGP 9 compiles Kotlin itself ("built-in Kotlin"), so org.jetbrains.kotlin.android is NOT
    // applied: doing so is an error in AGP 9. This plugin only adds the Compose compiler, and it
    // lifts Kotlin to 2.4.21.
    id("org.jetbrains.kotlin.plugin.compose")
}

// The repository root, where Cargo.toml lives. rootProject is clients/android.
val repoDir: File = rootProject.projectDir.resolve("../..").canonicalFile

/**
 * Builds core's UniFFI library (`brook-ffi`) for both Android ABIs and copies the two `.so`
 * files into the folder layout AGP packages (`<out>/arm64-v8a/`, `<out>/x86_64/`).
 *
 * Why a plain task and not a Gradle plugin for Rust: it is about thirty lines, nothing to keep
 * in step with each AGP release, and `with-ndk.sh` is shared with CI's Android clippy.
 */
abstract class RustLibs : DefaultTask() {
    @get:Inject
    abstract val exec: ExecOperations

    @get:Internal
    abstract val repoRoot: DirectoryProperty

    /** The NDK AGP resolved from `ndkVersion` below, so nobody sets ANDROID_NDK_HOME. */
    @get:Internal
    abstract val ndkDir: DirectoryProperty

    /** `$CARGO_TARGET_DIR` when set (a shared cache outside the repo), else `<repo>/target`. */
    @get:Internal
    abstract val cargoTargetDir: DirectoryProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    init {
        // Cargo decides what is stale; a no-op cargo run takes about a second. Telling Gradle
        // "up to date" from file hashes would only risk shipping an old library.
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val root = repoRoot.get().asFile
        val abis = mapOf("aarch64-linux-android" to "arm64-v8a", "x86_64-linux-android" to "x86_64")
        exec.exec {
            workingDir = root
            commandLine(
                root.resolve("clients/android/with-ndk.sh").path,
                ndkDir.get().asFile.path,
                "cargo", "build", "--locked", "--release", "-p", "brook-ffi",
                "--target", "aarch64-linux-android", "--target", "x86_64-linux-android",
            )
        }
        val out = outputDir.get().asFile
        out.deleteRecursively()
        for ((triple, abi) in abis) {
            val so = cargoTargetDir.get().asFile.resolve("$triple/release/libbrook_ffi.so")
            so.copyTo(out.resolve(abi).resolve(so.name), overwrite = true)
        }
    }
}

/**
 * Generates the Kotlin bindings with the repo's own `uniffi-bindgen` (the same pinned UniFFI as
 * the library), reading the API from the arm64 `.so`. "Library mode" reads the same metadata
 * from either ABI.
 */
abstract class KotlinBindings : DefaultTask() {
    @get:Inject
    abstract val exec: ExecOperations

    @get:Internal
    abstract val repoRoot: DirectoryProperty

    /** The folder `RustLibs` fills. Its use as an input also orders the two tasks. */
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val jniLibs: DirectoryProperty

    // The next two are inputs so an edit to a rename, or to the generator, reruns this task.
    // Otherwise a changed `uniffi.toml` would keep serving the old, stale Kotlin.
    @get:InputFile
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val renames: RegularFileProperty

    @get:InputFile
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val generatorSource: RegularFileProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun generate() {
        val out = outputDir.get().asFile
        // Delete first: a file the generator no longer writes must not linger and compile.
        out.deleteRecursively()
        exec.exec {
            // The repo root, because the generator finds `uniffi.toml` through `cargo metadata`,
            // which only works inside the workspace. (`--config` is NOT for this file.)
            workingDir = repoRoot.get().asFile
            commandLine(
                "cargo", "run", "--locked", "-p", "brook-ffi", "--features", "cli",
                "--bin", "uniffi-bindgen", "--",
                "generate", "--no-format", "--language", "kotlin",
                "--library", jniLibs.get().asFile.resolve("arm64-v8a/libbrook_ffi.so").path,
                "--out-dir", out.path,
            )
        }
    }
}

android {
    namespace = "me.madalin.brook"
    compileSdk = 37
    // Must match the installed package; AGP then strips the .so with that NDK.
    buildToolsVersion = "37.0.0"
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "me.madalin.brook"
        minSdk = 33
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    lint {
        warningsAsErrors = true
        abortOnError = true
    }
}

kotlin {
    jvmToolchain(21)
    compilerOptions {
        allWarningsAsErrors.set(true)
    }
}

val rustLibs = tasks.register<RustLibs>("rustLibs") {
    repoRoot.set(layout.dir(provider { repoDir }))
    ndkDir.set(androidComponents.sdkComponents.ndkDirectory)
    cargoTargetDir.set(
        layout.dir(
            providers.environmentVariable("CARGO_TARGET_DIR")
                .map { File(it) }
                .orElse(provider { repoDir.resolve("target") }),
        ),
    )
    outputDir.set(layout.buildDirectory.dir("generated/rustLibs/jniLibs"))
}

val kotlinBindings = tasks.register<KotlinBindings>("kotlinBindings") {
    repoRoot.set(layout.dir(provider { repoDir }))
    jniLibs.set(rustLibs.flatMap { it.outputDir })
    renames.set(repoDir.resolve("bindings/apple/uniffi.toml"))
    generatorSource.set(repoDir.resolve("bindings/apple/src/bin/uniffi-bindgen.rs"))
    outputDir.set(layout.buildDirectory.dir("generated/kotlinBindings"))
}

// Handing the task outputs to AGP as generated source folders makes AGP run the tasks, in the
// right order, before it packages or compiles. No `preBuild` dependsOn hook is needed.
androidComponents.onVariants { variant ->
    variant.sources.jniLibs?.addGeneratedSourceDirectory(rustLibs, RustLibs::outputDir)
    variant.sources.kotlin?.addGeneratedSourceDirectory(kotlinBindings, KotlinBindings::outputDir)
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.09.00"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.13.0")
    // What UniFFI's Kotlin calls native code through. The @aar carries libjnidispatch.so per ABI.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    // `SharedPreferences.edit {}`, which lint (UseKtx, warnings are errors) requires over
    // `edit().apply()`. Already on the classpath as `core`; this adds only the Kotlin extensions.
    implementation("androidx.core:core-ktx:1.19.1")
    // UniFFI's async functions are `suspend` functions.
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")

    testImplementation("junit:junit:4.13.2")
    // The plain jar, so JVM tests can load the generated classes (the @aar is Android-only).
    testImplementation("net.java.dev.jna:jna:5.19.1")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.11.0")

    // Device tests: the real Android Keystore, run by hand on an emulator (CI has none).
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
