/*
 * FluxBridge Android capture server - Gradle build (PLACEHOLDER).
 *
 * This is a minimal placeholder referenced by android-server/README.md. The
 * server is intentionally NOT a normal Android application (no AndroidManifest,
 * no Activity): it is a headless Java/Kotlin program launched on the device via
 * `adb shell app_process` (see the README). It only needs to be compiled to a
 * .jar and then dex'd, so the "application" plugin plus a fat-jar task is enough
 * for a real build. It is not built in the FluxBridge CI sandbox.
 *
 * To turn this into a buildable module, wire in the Android SDK (so the hidden
 * framework classes - SurfaceControl, InputManager, DisplayManager - are on the
 * compile classpath via a stub android.jar), then compile against it with
 * `compileOnly` so nothing from the SDK is bundled into the shipped jar.
 */

plugins {
    kotlin("jvm") version "1.9.24"
    application
}

group = "com.fluxbridge"
version = "0.1.0"

repositories {
    mavenCentral()
    google()
}

application {
    // Launched on the device as:
    //   adb shell CLASSPATH=... app_process / com.fluxbridge.server.Server
    mainClass.set("com.fluxbridge.server.Server")
}

dependencies {
    // The hidden framework APIs (SurfaceControl, InputManager, DisplayManager)
    // are provided by the on-device runtime, so they are `compileOnly` against a
    // framework stub jar and never packaged. Point this at a stub android.jar
    // (for example one produced for the target API level) in a real build:
    //
    //   compileOnly(files("libs/android-stub.jar"))
}

kotlin {
    jvmToolchain(17)
}

/*
 * A real build also needs a fat-jar (all classes in one jar) and a dexing step
 * (d8) so the artifact runs under ART. Those tasks are omitted from this
 * placeholder; see android-server/README.md for the full command sequence.
 */
