plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The version is a property so a release build can carry the tag it was cut
// from: `-PmcVersionName=0.2.0 -PmcVersionCode=200`. Left alone, it is whatever
// a local build has always been.
val mcVersionName = (project.findProperty("mcVersionName") as String?) ?: "0.1"
val mcVersionCode = (project.findProperty("mcVersionCode") as String?)?.toIntOrNull() ?: 1

// Signing is opt-in through the environment, so the keystore never lives in the
// repository and a checkout with no secrets still builds. Without it, a release
// build is unsigned and Android will not install it - which is why the workflow
// falls back to the debug build rather than shipping something that cannot be
// installed.
val keystore = System.getenv("MC_KEYSTORE")?.takeIf { File(it).exists() }

// `-Pmc.abi=<abi>[,<abi>]` picks the ABIs, for the Rust build *and* for what is
// packaged. Two lists that can disagree is how a release APK ends up carrying a
// stale library from an emulator build: measured here, an arm64-only build that
// still declared `native-code: 'arm64-v8a' 'x86_64'`.
val mcAbis = (project.findProperty("mc.abi") as String?)
    ?.split(",")
    ?.map(String::trim)
    ?.filter { it.isNotEmpty() }
    ?: listOf("arm64-v8a", "x86_64")

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
        versionCode = mcVersionCode
        versionName = mcVersionName
        ndk { abiFilters += mcAbis }
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

    if (keystore != null) {
        signingConfigs {
            create("release") {
                storeFile = File(keystore)
                storePassword = System.getenv("MC_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("MC_KEY_ALIAS")
                keyPassword = System.getenv("MC_KEY_PASSWORD") ?: System.getenv("MC_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release")
        }
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
    // A two-ABI Skia build is a long wait when the target device can only use
    // one of them; see `mcAbis` above.
    val abis = mcAbis

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
