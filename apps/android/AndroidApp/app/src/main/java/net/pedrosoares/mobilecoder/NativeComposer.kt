package net.pedrosoares.mobilecoder

import android.app.Activity
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowManager
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.PopupWindow

/**
 * The chat's message box, as a real Android view drawn over the Freya surface.
 *
 * Freya runs inside a NativeActivity, which has no InputConnection - the channel
 * an on-screen keyboard commits text through - so its own Input cannot receive
 * typing on a phone (measured on the API 36 emulator: keyboard open, input
 * focused, keystrokes lost). An EditText gets the full keyboard for free:
 * autocorrect, voice typing, paste, emoji.
 *
 * It lives in its own PopupWindow rather than in the activity's view tree. A
 * NativeActivity takes over its window's surface and input queue, so views added
 * with addContentView exist in the hierarchy but are never drawn and never get
 * touches (measured: uiautomator listed the EditText, the screen showed nothing).
 * A popup is a separate window with its own surface and input, which is also
 * what lets the keyboard attach to it.
 *
 * The Rust UI hides its own composer on Android and treats this bar's height as
 * part of the bottom safe area, so the transcript is never drawn beneath it.
 */
class NativeComposer(
    private val activity: Activity,
    /** A message for the agent. */
    private val onSubmit: (String) -> Unit,
    /** Raw input for the shell, control characters included. */
    private val onTerminalInput: (String) -> Unit,
    /** Stop the turn that is running. */
    private val onStop: () -> Unit,
    private val isBusy: () -> Boolean,
    /** What the Freya UI wants: 0 hidden, 1 chat, 2 terminal (mc-ui ComposerMode). */
    private val mode: () -> Int,
    /** Called when the bar appears, disappears or changes size. */
    private val onVisibilityChanged: () -> Unit,
) {
    private companion object {
        const val MODE_HIDDEN = 0
        const val MODE_CHAT = 1
        const val MODE_TERMINAL = 2
    }

    private var currentMode = MODE_CHAT
    private fun dp(value: Float) = TypedValue.applyDimension(
        TypedValue.COMPLEX_UNIT_DIP, value, activity.resources.displayMetrics,
    ).toInt()

    // Colours from mc-ui/src/theme.rs, so the native bar matches the Freya UI.
    private val surface = Color.rgb(22, 34, 38)
    private val ink = Color.rgb(223, 232, 234)
    private val muted = Color.rgb(133, 150, 155)
    private val accent = Color.rgb(91, 187, 176)
    private val danger = Color.rgb(219, 124, 109)
    private val ground = Color.rgb(12, 19, 21)

    private val input = EditText(activity).apply {
        hint = "Ask the agent to build something"
        setTextColor(ink)
        setHintTextColor(muted)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        // Multi-line: Enter inserts a newline, Send sends - the usual chat
        // behaviour, and prompts for a coding agent are often more than a line.
        inputType = InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
        imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI
        maxLines = 5
        minHeight = dp(44f)
        setOnEditorActionListener { _, actionId, _ ->
            if (actionId == EditorInfo.IME_ACTION_SEND) {
                submit()
                true
            } else {
                false
            }
        }
        setPadding(dp(12f), dp(8f), dp(12f), dp(8f))
        background = GradientDrawable().apply {
            cornerRadius = dp(10f).toFloat()
            setColor(ground)
        }
    }

    private val send = Button(activity).apply {
        text = "Send"
        isAllCaps = false
        setTextColor(ground)
        backgroundTintList = ColorStateList.valueOf(accent)
        setOnClickListener {
            // While the agent works this button stops it; otherwise it sends.
            if (currentMode == MODE_CHAT && isBusy()) onStop() else submit()
        }
    }

    private val inputRow = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        addView(input, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        addView(send, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
            marginStart = dp(8f)
        })
    }

    /**
     * Keys a phone keyboard does not have, which a shell cannot do without -
     * the same idea as Termux's extra-keys row. Terminal mode only.
     */
    private val extraKeys = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        visibility = View.GONE
        setPadding(0, 0, 0, dp(6f))
        listOf(
            "Esc" to "\u001b",
            "Tab" to "\t",
            "Ctrl-C" to "\u0003",
            "↑" to "\u001b[A",
            "↓" to "\u001b[B",
        ).forEach { (caption, sequence) ->
            addView(
                Button(activity).apply {
                    text = caption
                    isAllCaps = false
                    setTextColor(ink)
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                    minHeight = 0
                    minimumHeight = 0
                    minWidth = 0
                    minimumWidth = 0
                    setPadding(dp(4f), dp(6f), dp(4f), dp(6f))
                    backgroundTintList = ColorStateList.valueOf(ground)
                    setOnClickListener { onTerminalInput(sequence) }
                },
                LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                    marginEnd = dp(4f)
                },
            )
        }
    }

    /** The bar itself. Its bottom margin tracks the keyboard and navigation bar. */
    val view = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        setBackgroundColor(surface)
        setPadding(dp(8f), dp(6f), dp(8f), dp(6f))
        addView(extraKeys, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        addView(inputRow, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
    }

    private val popup = PopupWindow(
        view,
        ViewGroup.LayoutParams.MATCH_PARENT,
        ViewGroup.LayoutParams.WRAP_CONTENT,
        // Focusable, or the keyboard cannot attach to the EditText.
        true,
    ).apply {
        inputMethodMode = PopupWindow.INPUT_METHOD_NEEDED
        // Positioned by hand from the measured insets, so the system must not
        // also shift it for the keyboard.
        softInputMode = WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING
        // A focusable popup is modal by default: it would swallow every touch
        // on the chat above it and close on the first one.
        isTouchModal = false
        isOutsideTouchable = false
    }

    private var navInsetPx = 0
    private var imeInsetPx = 0

    /** Height the bar covers, including whatever it sits on. */
    val coveredBottomPx: Int
        get() = if (popup.isShowing) {
            maxOf(navInsetPx, imeInsetPx) + view.height
        } else {
            navInsetPx
        }

    /** Kept so the bar can be shown again after being dismissed. */
    private var anchor: View? = null

    /**
     * Held down while a dialog is up.
     *
     * The bar's popup window is focusable - it has to be, or the keyboard would
     * have nothing to attach to - and a focusable popup keeps the input focus
     * even when a dialog opens above it. Typing into the settings form landed
     * in the message box instead (measured on the emulator). So the bar steps
     * aside for as long as a dialog is showing.
     */
    private var suspended = false

    /** Take the bar away (and give up the keyboard) while a dialog is open. */
    fun setSuspended(value: Boolean) {
        if (value == suspended) return
        suspended = value
        applyVisibility(!suspended && mode() != MODE_HIDDEN)
    }

    /**
     * Show the bar. Must run once the activity window is attached.
     *
     * The keyboard inset is read from the popup's own window, not the
     * activity's: once the EditText has focus, the IME targets this window and
     * only it is told how much the keyboard covers.
     */
    fun show(anchor: View, onInsetsChanged: () -> Unit) {
        this.anchor = anchor
        view.setOnApplyWindowInsetsListener { v, insets ->
            val ime = insets.getInsets(WindowInsets.Type.ime()).bottom
            if (ime != imeInsetPx) {
                imeInsetPx = ime
                reposition()
                onInsetsChanged()
            }
            v.onApplyWindowInsets(insets)
        }
        popup.showAtLocation(anchor, Gravity.BOTTOM or Gravity.START, 0, maxOf(navInsetPx, imeInsetPx))
    }

    private fun submit() {
        if (currentMode == MODE_TERMINAL) {
            // Not trimmed, and empty is fine: a bare Enter is a real keystroke
            // in a shell (a fresh prompt, confirming a pager).
            onTerminalInput(input.text.toString() + "\r")
            input.text.clear()
            return
        }
        val text = input.text.toString().trim()
        if (text.isEmpty() || isBusy()) return
        onSubmit(text)
        input.text.clear()
    }

    /** Reconfigure the bar for chat or terminal input. */
    private fun applyMode(newMode: Int) {
        if (newMode == currentMode || newMode == MODE_HIDDEN) return
        currentMode = newMode
        val terminal = newMode == MODE_TERMINAL
        extraKeys.visibility = if (terminal) View.VISIBLE else View.GONE
        if (terminal) {
            // One line, sent by the keyboard's own Enter key; no autocorrect
            // mangling commands.
            input.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            input.imeOptions = EditorInfo.IME_ACTION_SEND or EditorInfo.IME_FLAG_NO_EXTRACT_UI
            input.maxLines = 1
        } else {
            input.inputType = InputType.TYPE_CLASS_TEXT or
                InputType.TYPE_TEXT_FLAG_MULTI_LINE or
                InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            input.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI
            input.maxLines = 5
        }
        // The keyboard caches the input type; restart it so the change shows.
        activity.getSystemService(InputMethodManager::class.java)?.restartInput(input)
        onVisibilityChanged()
    }

    /** The navigation bar height, from the activity window. */
    fun setNavigationInset(px: Int) {
        if (px != navInsetPx) {
            navInsetPx = px
            reposition()
        }
    }

    private fun reposition() {
        if (popup.isShowing) {
            popup.update(0, maxOf(navInsetPx, imeInsetPx), -1, -1)
        }
    }

    fun dismiss() {
        stopTracking()
        popup.dismiss()
    }

    /**
     * Show or remove the bar.
     *
     * Dismissed, not hidden. Setting the content to GONE hid the views but left
     * the popup *window* in place, still drawing its last frame - and sliding it
     * down over the navigation bar (measured on the emulator: a stale Send
     * button at the bottom edge of the Files pane).
     */
    private fun applyVisibility(wanted: Boolean) {
        val wanted = wanted && !suspended
        if (wanted == popup.isShowing) return
        if (wanted) {
            val target = anchor ?: return
            popup.showAtLocation(target, Gravity.BOTTOM or Gravity.START, 0, maxOf(navInsetPx, imeInsetPx))
        } else {
            // Leaving the chat mid-sentence should not leave the keyboard up
            // over a pane that cannot use it. The draft is kept for coming back.
            val imm = activity.getSystemService(InputMethodManager::class.java)
            imm?.hideSoftInputFromWindow(input.windowToken, 0)
            input.clearFocus()
            imeInsetPx = 0
            popup.dismiss()
        }
        onVisibilityChanged()
    }

    /**
     * Drives the polling loop. Not `view.postDelayed`: once the popup is
     * dismissed its view is detached, and a detached view holds posted
     * runnables until it is attached again - so the loop that should re-show
     * the bar would wait for the bar to be shown (measured: it never came back).
     */
    private val handler = Handler(Looper.getMainLooper())

    private val refresh = object : Runnable {
        override fun run() {
            val wanted = mode()
            applyVisibility(wanted != MODE_HIDDEN)
            applyMode(wanted)
            if (currentMode == MODE_TERMINAL) {
                // The shell is independent of the agent: never blocked by a turn.
                send.isEnabled = true
                send.alpha = 1f
                send.text = "Send"
            send.backgroundTintList = ColorStateList.valueOf(accent)
            input.hint = "Type a command"
                handler.postDelayed(this, 250)
                return
            }
            val busy = isBusy()
            // Enabled either way: as Send when idle, as Stop while working.
            send.isEnabled = true
            send.alpha = 1f
            send.text = if (busy) "Stop" else "Send"
            send.backgroundTintList = ColorStateList.valueOf(if (busy) danger else accent)
            input.hint = if (busy) "Working… (Stop to interrupt)" else "Ask the agent to build something"
            handler.postDelayed(this, 250)
        }
    }

    /**
     * Keep Send in step with the agent. Polled: the busy flag changes on a Rust
     * thread, and a quarter-second lag on a button state is not worth a JNI
     * callback into the UI thread.
     */
    fun startTrackingBusy() = handler.post(refresh)

    /** Stop polling; call when the activity goes away. */
    fun stopTracking() = handler.removeCallbacks(refresh)
}
