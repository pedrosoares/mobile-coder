package net.pedrosoares.mobilecoder.execprobe

import android.os.Bundle
import android.widget.ScrollView
import android.widget.TextView
import android.app.Activity
import android.util.TypedValue
import java.io.File
import kotlin.concurrent.thread

/**
 * Runs the execution probe and shows the report.
 *
 * The probe has to execute inside this process: `adb shell` runs in a different
 * SELinux domain where app-data execution rules do not apply, so a shell-based
 * test would pass and tell us nothing.
 */
class MainActivity : Activity() {

    private external fun runProbe(
        filesDir: String,
        nativeLibDir: String,
        targetSdk: Int,
    ): String

    companion object {
        init { System.loadLibrary("exec_probe") }

        /** Fixed name the Rust side looks for, whichever ABI supplied it. */
        private const val STAGED_ROOTFS = "alpine-minirootfs.tar"
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val output = TextView(this).apply {
            setTextIsSelectable(true)
            setTypeface(android.graphics.Typeface.MONOSPACE)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setPadding(24, 24, 24, 24)
            text = "Running probe…"
        }
        setContentView(ScrollView(this).apply { addView(output) })

        // Extraction and process spawning are slow enough to block a frame.
        thread {
            stageRootfsAsset()
            val report = runProbe(
                filesDir.absolutePath,
                applicationInfo.nativeLibraryDir,
                applicationInfo.targetSdkVersion,
            )
            runOnUiThread { output.text = report }
        }
    }

    /**
     * Copy the rootfs matching this device's ABI out of the APK, under a fixed
     * name. Assets live inside the APK and are not visible on the filesystem, so
     * Rust cannot open them directly.
     *
     * The guest binaries must match the CPU: a musl aarch64 busybox on an x86_64
     * emulator would fail for a reason that has nothing to do with SELinux, and
     * would quietly invalidate the whole result.
     */
    private fun stageRootfsAsset() {
        val target = File(filesDir, STAGED_ROOTFS)
        if (target.exists() && target.length() > 0) return

        val abi = android.os.Build.SUPPORTED_ABIS.firstOrNull() ?: "arm64-v8a"
        val asset = "alpine-minirootfs-$abi.tar"
        try {
            assets.open(asset).use { input ->
                target.outputStream().use { output -> input.copyTo(output) }
            }
            android.util.Log.i("mc-exec-probe", "staged $asset for abi=$abi")
        } catch (e: Exception) {
            // Not fatal: the probe reports those checks as SKIPPED, which is more
            // useful than crashing here.
            android.util.Log.w("mc-exec-probe", "no rootfs asset for abi=$abi ($asset): $e")
        }
    }
}
