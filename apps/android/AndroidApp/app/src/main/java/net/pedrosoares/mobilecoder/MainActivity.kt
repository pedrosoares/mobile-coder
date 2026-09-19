package net.pedrosoares.mobilecoder

import android.app.NativeActivity
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.os.Build
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

    /** Stops the running turn. Implemented in agent_task.rs. */
    private external fun nativeStopTurn()

    /** True once when the user taps the settings button. Implemented in agent_task.rs. */
    private external fun nativeTakeSettingsRequest(): Boolean

    /** Text the app wants on the clipboard, or "". Implemented in agent_task.rs. */
    private external fun nativeTakeClipboard(): String

    /** Drops the key from the running agent. Implemented in credentials.rs. */
    private external fun nativeClearApiKey()

    /** Hands the GitHub token to the Git pane. Implemented in credentials.rs. */
    private external fun nativeSetGithubToken(token: String)

    /** Which field the UI is waiting for, 0 for none. Implemented in agent_task.rs. */
    private external fun nativeTakePromptRequest(): Int

    /** The title, hint and flags for a prompt, as "title\u0000hint\u00000|1\u00000|1". */
    private external fun nativePromptSpec(kind: Int): String

    /** Hands back what the user typed. Implemented in agent_task.rs. */
    private external fun nativeAnswerPrompt(kind: Int, text: String)

    /** True once when the user signs out of GitHub. Implemented in agent_task.rs. */
    private external fun nativeTakeGithubForget(): Boolean

    /** What is running, or "" for nothing. Implemented in agent_task.rs. */
    private external fun nativeWorkSummary(): String

    /**
     * Keep the app out of the freezer while its processes have work to do.
     *
     * Android freezes a cached app, and the sandbox is inside this app: a
     * server stops answering, a build stops compiling, a job stops counting.
     * A foreground service is the only thing that prevents it - so one runs
     * exactly while there is work, plus whenever the user has pinned it on.
     */
    private fun syncKeepAlive() {
        val summary = nativeWorkSummary()
        val pinned = getSharedPreferences(ENDPOINT_PREFS, Context.MODE_PRIVATE)
            .getBoolean(PREF_KEEP_AWAKE, false)
        if (summary.isEmpty()) {
            // Nothing running: a Stop pressed earlier should not mute the next
            // job too.
            KeepAlive.workEnded()
        }
        KeepAlive.sync(
            context = this,
            wanted = summary.isNotEmpty() || pinned,
            summary = summary.ifEmpty { "Keeping the sandbox awake" },
        )
    }

    /** Ask to show the keep-alive notification, on the releases that require it. */
    private fun requestNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        // Denied is survivable: the service still runs and still keeps the
        // sandbox alive, the user just cannot see or stop it from the shade.
        requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1)
    }

    /** Ask for one line of text on behalf of the Git pane. */
    private fun openPrompt(kind: Int) {
        // The wording lives in Rust with the screen that asks, so the two
        // cannot drift apart.
        val spec = nativePromptSpec(kind).split("\u0000")
        if (spec.size < 4) return
        composer.setSuspended(true)
        TextPromptDialog.show(
            activity = this,
            title = spec[0],
            hint = spec[1],
            secret = spec[2] == "1",
            multiline = spec[3] == "1",
            onText = { text ->
                if (text.isNotBlank()) {
                    // A token is stored here and nowhere else; the rest is just
                    // text on its way to the pane.
                    if (kind == PROMPT_GITHUB_TOKEN) {
                        KeyVault.storeGithub(this, text.trim())
                    }
                    nativeAnswerPrompt(kind, text)
                }
            },
            onClosed = { composer.setSuspended(false) },
        )
    }

    /**
     * Anything the Freya side cannot do itself, polled from the UI thread.
     *
     * Both of these must happen here: a dialog and the clipboard belong to the
     * Activity's thread, and Freya runs elsewhere. A quarter-second is under
     * the threshold where a tap feels unacknowledged.
     */
    private fun watchNativeRequests() {
        val handler = android.os.Handler(android.os.Looper.getMainLooper())
        handler.post(object : Runnable {
            override fun run() {
                if (nativeTakeSettingsRequest()) openSettings()
                val prompt = nativeTakePromptRequest()
                if (prompt != 0) openPrompt(prompt)
                if (nativeTakeGithubForget()) {
                    KeyVault.clearGithub(this@MainActivity)
                    Log.i(TAG, "github token deleted from this device")
                }
                val copied = nativeTakeClipboard()
                if (copied.isNotEmpty()) putOnClipboard(copied)
                syncKeepAlive()
                handler.postDelayed(this, 250)
            }
        })
    }

    private fun putOnClipboard(text: String) {
        val clipboard = getSystemService(android.content.ClipboardManager::class.java)
        if (clipboard == null) {
            Log.w(TAG, "no clipboard service; copy dropped")
            return
        }
        clipboard.setPrimaryClip(android.content.ClipData.newPlainText("mobile-coder", text))
        // Android 13+ shows its own copy confirmation; older releases show
        // nothing, so say it once here rather than twice there.
        if (android.os.Build.VERSION.SDK_INT < android.os.Build.VERSION_CODES.TIRAMISU) {
            android.widget.Toast
                .makeText(this, "Copied", android.widget.Toast.LENGTH_SHORT)
                .show()
        }
    }

    private fun openSettings() {
        composer.setSuspended(true)
        SettingsDialog.show(
            activity = this,
            hasKey = KeyVault.load(this) != null,
            onApplied = { apiKey, baseUrl, model ->
                apiKey?.let {
                    KeyVault.store(this, it)
                    nativeSetApiKey(it)
                }
                nativeSetEndpoint(baseUrl, model)
                // credentials.rs logs which endpoint and model took effect; no
                // point printing the same pair twice.
                Log.i(TAG, "settings applied")
            },
            onCleared = {
                KeyVault.clear(this)
                nativeClearApiKey()
                Log.i(TAG, "api key forgotten")
            },
            onClosed = { composer.setSuspended(false) },
        )
    }

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
            onStop = { nativeStopTurn() },
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

        /** Matches `mc_ui::prompt::Prompt`; the numbers are the interface. */
        private const val PROMPT_GITHUB_TOKEN = 1

        /** Where the endpoint, model and keep-awake switch are remembered. */
        const val ENDPOINT_PREFS = "endpoint"
        const val PREF_KEEP_AWAKE = "keep_awake"

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
        val prefs = getSharedPreferences(ENDPOINT_PREFS, MODE_PRIVATE)
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
        // Before the UI starts, so the Git pane opens already signed in rather
        // than asking for a token the device already has.
        KeyVault.loadGithub(this)?.let { nativeSetGithubToken(it) }
        runPromptFromIntent()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        watchDnsServers()
        watchNativeRequests()
        requestNotificationPermission()
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
