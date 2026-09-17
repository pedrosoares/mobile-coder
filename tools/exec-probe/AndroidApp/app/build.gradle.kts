plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "net.pedrosoares.mobilecoder.execprobe"
    compileSdk = 36

    defaultConfig {
        applicationId = "net.pedrosoares.mobilecoder.execprobe"
        minSdk = 24

        // The whole point of this probe. Do not lower it to make a test pass -
        // lowering it is the thing we are trying to find out whether we can avoid.
        targetSdk = 36

        versionCode = 1
        versionName = "0.1"
        // arm64-v8a is the real target; x86_64 lets the probe run natively on an
        // emulator, where the SELinux and linker questions answer just as truthfully.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    // cargo-ndk writes the Rust cdylib here; fetch-assets.sh drops libproot.so
    // alongside it. Native libraries are executable regardless of targetSdk,
    // which is exactly what check `nativelib-exec` verifies.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    packaging {
        jniLibs {
            // We need real files on disk to exec, not libraries mapped straight
            // out of the APK.
            useLegacyPackaging = true
        }
    }

    buildTypes {
        release { isMinifyEnabled = false }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {}
