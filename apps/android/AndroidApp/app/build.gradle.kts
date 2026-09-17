plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "net.pedrosoares.mobilecoder"
    compileSdk = 36

    defaultConfig {
        applicationId = "net.pedrosoares.mobilecoder"
        // Freya's Android support targets 31+; below that the winit/NativeActivity
        // surface handling upstream relies on is not available.
        minSdk = 31
        // Measured as viable: proot runs a Linux userland here with SELinux
        // enforcing. See docs/EXEC-PROBE.md before lowering this.
        targetSdk = 36
        versionCode = 1
        versionName = "0.1"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    // cargo-ndk writes libmobile_coder.so here; tools/fetch-proot.sh drops
    // proot and its libraries alongside. Everything in this directory is
    // executable on device at any targetSdkVersion - which is the only reason
    // a Linux userland is possible at all.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    packaging {
        // Real files on disk, not libraries mapped straight out of the APK -
        // proot has to be exec'd, and its loader has to be openable by path.
        jniLibs { useLegacyPackaging = true }
    }

    buildTypes {
        release { isMinifyEnabled = false }
        debug { isJniDebuggable = true }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

// Build the Rust cdylib as part of the Gradle build, so `./gradlew assembleDebug`
// is self-contained.
//
// ANDROID_JAR is not optional: `mundy`, a transitive dependency of Freya, locates
// android.jar in its build script and panics without it - even for `cargo check`.
tasks.register<Exec>("buildRustLibrary") {
    workingDir("../../../..")
    val androidHome = System.getenv("ANDROID_HOME") ?: System.getenv("ANDROID_SDK_ROOT") ?: ""
    environment("ANDROID_HOME", androidHome)
    environment("ANDROID_JAR", "$androidHome/platforms/android-36/android.jar")
    // -Pmc.abi=<abi> builds a single ABI. A two-ABI Skia build is a long wait
    // when the target device can only use one of them.
    val abis = (project.findProperty("mc.abi") as String?)
        ?.split(",")
        ?.map(String::trim)
        ?.filter { it.isNotEmpty() }
        ?: listOf("arm64-v8a", "x86_64")

    commandLine(
        buildList {
            addAll(listOf("cargo", "ndk", "-o", "apps/android/AndroidApp/app/src/main/jniLibs"))
            abis.forEach { addAll(listOf("-t", it)) }
            // Ship the C++ runtime next to our library. The terminal stack
            // includes simdutf (C++), which links libc++_shared; without the
            // .so in the APK the app dies at load with "dlopen failed:
            // library libc++_shared.so not found".
            add("--link-libcxx-shared")
            addAll(listOf("--platform", "31", "build", "--release", "-p", "android"))
        }
    )
}

tasks.named("preBuild") { dependsOn("buildRustLibrary") }

dependencies {}
