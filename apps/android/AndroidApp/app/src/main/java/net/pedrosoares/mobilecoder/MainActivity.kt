package net.pedrosoares.mobilecoder

import android.app.NativeActivity
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.os.Bundle
import android.util.Log
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets

/**
 * Host for the Rust/Freya renderer.
 *
 * All the UI lives in `android_main` on the native side; this exists only to give
 * the native surface keyboard focus. Without the focus fix below, the surface
 * never becomes focusable and the soft keyboard will not open for text input -
 * which on a phone means the app cannot be typed into at all.
 */
class MainActivity : NativeActivity() {

    private fun findNativeSurfaceView(view: View): View? {
        if (view is SurfaceView) return view
        if (view is ViewGroup) {
            for (i in 0 until view.childCount) {
                findNativeSurfaceView(view.getChildAt(i))?.let { return it }
            }
        }
        return null
    }

    /** Hands the decrypted API key to the Rust side. Implemented in credentials.rs. */
    private external fun nativeSetApiKey(key: String)

    /** Sets the Messages API endpoint and model. Implemented in credentials.rs. */
    private external fun nativeSetEndpoint(baseUrl: String, model: String)

    /** Pushes the current network's DNS servers. Implemented in network.rs. */
    private external fun nativeSetDnsServers(servers: String)

    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    /**
     * Keep the guest's DNS in step with the phone's network.
     *
     * Android has no /etc/resolv.conf; each network carries its own DNS servers.
     * The guest needs them written into its own resolv.conf, and they change
     * whenever the phone moves between Wi-Fi and cellular - so this listens for
     * changes rather than reading the servers once.
     */
    private fun watchDnsServers() {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        fun push(props: LinkProperties?) {
            val servers = props?.dnsServers.orEmpty().joinToString(",") { it.hostAddress ?: "" }
            nativeSetDnsServers(servers)
        }
        push(cm.getLinkProperties(cm.activeNetwork))

        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, props: LinkProperties) = push(props)
        }
        cm.registerDefaultNetworkCallback(callback)
        networkCallback = callback
    }

    override fun onDestroy() {
        if (::composer.isInitialized) composer.dismiss()
        networkCallback?.let {
            (getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager).unregisterNetworkCallback(it)
        }
        networkCallback = null
        super.onDestroy()
    }

    /** Space for system bars and the keyboard, in logical px. Implemented in agent_task.rs. */
    private external fun nativeSetSafeArea(top: Float, bottom: Float)

    /** Whether an agent turn is running. Implemented in agent_task.rs. */
    private external fun nativeIsBusy(): Boolean

    /** 0 hidden, 1 chat, 2 terminal. Implemented in agent_task.rs. */
    private external fun nativeComposerMode(): Int

    /** Raw input for the Terminal pane's shell. Implemented in agent_task.rs. */
    private external fun nativeTerminalInput(text: String)

    private lateinit var composer: NativeComposer

    /** Last measured status bar inset in px. */
    private var topInsetPx = 0

    /**
     * Tell the Rust UI how much space is covered at each edge. The bottom is the
     * keyboard or navigation bar *plus* the native message box sitting on it.
     */
    private fun reportSafeArea() {
        val density = resources.displayMetrics.density
        nativeSetSafeArea(topInsetPx / density, composer.coveredBottomPx / density)
    }

    private fun installComposer() {
        composer = NativeComposer(
            activity = this,
            onSubmit = { text -> nativeRunPrompt(text) },
            onTerminalInput = { text -> nativeTerminalInput(text) },
            isBusy = { nativeIsBusy() },
            mode = { nativeComposerMode() },
            onVisibilityChanged = { reportSafeArea() },
        )
        // Its height changes with the number of lines typed.
        composer.view.addOnLayoutChangeListener { _, _, top, _, bottom, _, oldTop, _, oldBottom ->
            if (bottom - top != oldBottom - oldTop) reportSafeArea()
        }
        // A popup needs the activity window to be attached first.
        window.decorView.post {
            composer.show(window.decorView) { reportSafeArea() }
            composer.startTrackingBusy()
        }
    }

    /**
     * Report the system bar and keyboard insets to the UI.
     *
     * Since targetSdk 35 Android draws apps edge to edge and ignores
     * adjustResize, so nothing moves out of the way of the status bar or the
     * keyboard unless the app does it. Freya has no inset API, so the values are
     * measured here and passed down. The bottom inset is the larger of the
     * navigation bar and the keyboard, which is what keeps the message box
     * visible while typing.
     *
     * Converted to logical pixels with the display density - the same scale
     * factor winit reports to Freya.
     */
    private fun watchInsets() {
        window.decorView.setOnApplyWindowInsetsListener { view, insets ->
            val bars = insets.getInsets(WindowInsets.Type.systemBars())
            val ime = insets.getInsets(WindowInsets.Type.ime())
            topInsetPx = bars.top
            composer.setNavigationInset(maxOf(bars.bottom, ime.bottom))
            reportSafeArea()
            // Keep the platform's own handling too.
            view.onApplyWindowInsets(insets)
        }
        window.decorView.requestApplyInsets()
    }

    /** Runs one agent turn. Implemented in agent_task.rs. */
    private external fun nativeRunPrompt(prompt: String)

    companion object {
        private const val TAG = "mobile-coder"

        // NativeActivity loads this itself, but we need it before super.onCreate
        // returns so the key can be installed as early as possible. loadLibrary
        // is idempotent.
        init { System.loadLibrary("mobile_coder") }
    }

    /**
     * Take an API key from the launch intent, if one was supplied, and seal it.
     *
     * There is no settings UI yet, so this is how a key gets in:
     *
     *   adb shell am start -n net.pedrosoares.mobilecoder/.MainActivity \
     *     --es api_key sk-ant-...
     *
     * The value is stored encrypted and the intent extra is dropped immediately,
     * so a later relaunch does not need it. Note that intent extras are visible
     * to `adb` and to anything that can see the launch - fine for a developer
     * setting up their own device, not a way to ship secrets.
     */
    private fun captureApiKeyFromIntent() {
        val supplied = intent?.getStringExtra("api_key")?.takeIf { it.isNotBlank() } ?: return
        KeyVault.store(this, supplied.trim())
        intent.removeExtra("api_key")
    }

    /**
     * Run one agent turn if the launch intent carried a prompt.
     *
     * A stand-in for the chat UI, so the agent path is exercisable now:
     *
     *   adb shell am start -n net.pedrosoares.mobilecoder/.MainActivity \
     *     --es prompt "list the files in /root"
     *
     * Delayed, because the rootfs may still be installing on first launch and the
     * agent's tools are useless without a working sandbox.
     */
    private fun runPromptFromIntent() {
        val prompt = intent?.getStringExtra("prompt")?.takeIf { it.isNotBlank() } ?: return
        intent.removeExtra("prompt")
        Log.i(TAG, "prompt queued: $prompt")
        window.decorView.postDelayed({ nativeRunPrompt(prompt.trim()) }, 1_500)
    }

    /**
     * Handle a key or prompt delivered while the app is already running.
     *
     * Without this, a second `am start` is routed to the existing instance,
     * `onCreate` never runs, and the intent is silently dropped - the app looks
     * like it ignored the request.
     */
    override fun onNewIntent(newIntent: Intent) {
        super.onNewIntent(newIntent)
        // getIntent() is what the handlers below read, so it must be updated.
        setIntent(newIntent)
        applyIntent()
    }

    /**
     * Endpoint and model overrides, e.g. for LM Studio on the host machine:
     *
     *   --es base_url http://10.0.2.2:1234 --es model qwen/qwen3.8-27b
     *
     * Not secret, so plain SharedPreferences. An empty value resets to the
     * default (Anthropic's API). `10.0.2.2` is how the emulator reaches the
     * host's localhost; a physical phone needs the machine's LAN address.
     */
    private fun applyEndpoint() {
        val prefs = getSharedPreferences("endpoint", MODE_PRIVATE)
        val edit = prefs.edit()
        intent?.getStringExtra("base_url")?.let { edit.putString("base_url", it.trim()); intent.removeExtra("base_url") }
        intent?.getStringExtra("model")?.let { edit.putString("model", it.trim()); intent.removeExtra("model") }
        edit.apply()
        nativeSetEndpoint(prefs.getString("base_url", "") ?: "", prefs.getString("model", "") ?: "")
    }

    /** Apply whatever the current intent asks for: a key, a prompt, or neither. */
    private fun applyIntent() {
        applyEndpoint()
        captureApiKeyFromIntent()
        when (val key = KeyVault.load(this)) {
            null -> Log.i(TAG, "no api key stored; the agent will stay idle")
            else -> nativeSetApiKey(key)
        }
        runPromptFromIntent()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        watchDnsServers()
        installComposer()
        watchInsets()
        applyIntent()
        // Posted, because the native surface does not exist yet during onCreate.
        // Focusable so hardware keys still reach Freya; not requestFocus(), which
        // would take focus from the message box on every launch.
        window.decorView.post {
            findNativeSurfaceView(window.decorView)?.apply {
                isFocusable = true
                isFocusableInTouchMode = true
            }
        }
    }
}
