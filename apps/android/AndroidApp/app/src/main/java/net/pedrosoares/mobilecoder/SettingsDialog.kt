package net.pedrosoares.mobilecoder

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.graphics.Color
import android.text.InputType
import android.util.TypedValue
import android.view.ViewGroup
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView

/**
 * Where the model is configured, from the phone itself.
 *
 * A native dialog rather than a Freya screen, for the same reason the message
 * box is native: Freya receives no on-screen keyboard text inside a
 * NativeActivity, so a form drawn by Freya could not be typed into. Until this
 * existed the app could only be configured over adb from a computer, which
 * defeats the point of a coding agent that runs on a phone.
 */
object SettingsDialog {

    private const val PREFS = "endpoint"

    private fun dp(activity: Activity, value: Float) = TypedValue.applyDimension(
        TypedValue.COMPLEX_UNIT_DIP, value, activity.resources.displayMetrics,
    ).toInt()

    private fun field(
        activity: Activity,
        label: String,
        hint: String,
        value: String,
        password: Boolean = false,
    ): Pair<LinearLayout, EditText> {
        val caption = TextView(activity).apply {
            text = label
            setTextColor(Color.rgb(133, 150, 155))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
        }
        val input = EditText(activity).apply {
            setText(value)
            setHint(hint)
            setTextColor(Color.rgb(223, 232, 234))
            setHintTextColor(Color.rgb(100, 112, 117))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            inputType = if (password) {
                InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            } else {
                // No autocorrect: these are URLs, model ids and keys.
                InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            }
            maxLines = 1
        }
        val row = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            addView(caption)
            addView(input)
        }
        return row to input
    }

    /**
     * Show the form. [onApplied] receives the values to hand to the native side.
     *
     * The key is write-only here: the stored one is never shown, only whether
     * there is one. Leaving the field empty keeps it.
     */
    fun show(
        activity: Activity,
        hasKey: Boolean,
        onApplied: (apiKey: String?, baseUrl: String, model: String) -> Unit,
        onCleared: () -> Unit,
        onClosed: () -> Unit,
    ) {
        @Suppress("NAME_SHADOWING")
        val prefs = activity.getSharedPreferences(MainActivity.ENDPOINT_PREFS, Context.MODE_PRIVATE)
        val (keyRow, keyInput) = field(
            activity,
            if (hasKey) "API key (stored — leave empty to keep)" else "API key",
            if (hasKey) "••••••••" else "sk-ant-…",
            "",
            password = true,
        )
        val (urlRow, urlInput) = field(
            activity,
            "Endpoint (empty = Anthropic API)",
            "http://192.168.1.10:1234",
            prefs.getString("base_url", "") ?: "",
        )
        val (modelRow, modelInput) = field(
            activity,
            "Model (empty = claude-opus-5)",
            "claude-opus-5",
            prefs.getString("model", "") ?: "",
        )

        // Kept on even with nothing running: a server started from the Terminal
        // is invisible to the app - it cannot tell a shell that is serving from
        // one sitting at a prompt - so the only honest way to keep that alive is
        // to let the user say so.
        val keepAwake = android.widget.CheckBox(activity).apply {
            text = "Keep the sandbox running in the background"
            setTextColor(Color.rgb(223, 232, 234))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            isChecked = prefs.getBoolean(MainActivity.PREF_KEEP_AWAKE, false)
        }
        val keepAwakeNote = TextView(activity).apply {
            text = "Without this, Android freezes everything here the moment the app leaves the " +
                "screen. A turn or a background job keeps it awake on its own."
            setTextColor(Color.rgb(133, 150, 155))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
        }
        val keepAwakeRow = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            addView(keepAwake)
            addView(keepAwakeNote)
        }

        val layout = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(activity, 24f), dp(activity, 16f), dp(activity, 24f), 0)
            listOf(keyRow, urlRow, modelRow, keepAwakeRow).forEach {
                addView(it, LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                ).apply { topMargin = dp(activity, 12f) })
            }
        }

        // Scrolling, because the keyboard covers the lower half of a phone
        // screen and Save must stay reachable with a field being edited.
        val scroller = android.widget.ScrollView(activity).apply { addView(layout) }

        val dialog = AlertDialog.Builder(activity)
            .setTitle("Model settings")
            .setView(scroller)
            .setPositiveButton("Save") { _, _ ->
                val typed = keyInput.text.toString().trim()
                val url = urlInput.text.toString().trim()
                val model = modelInput.text.toString().trim()
                prefs.edit()
                    .putString("base_url", url)
                    .putString("model", model)
                    .putBoolean(MainActivity.PREF_KEEP_AWAKE, keepAwake.isChecked)
                    .apply()
                onApplied(typed.ifEmpty { null }, url, model)
            }
            .setNeutralButton("Forget key") { _, _ -> onCleared() }
            .setNegativeButton("Cancel", null)
            // Every way out, including the back button: whoever stepped aside
            // for the dialog has to be told it is gone.
            .setOnDismissListener { onClosed() }
            .create()
        // Sit at the top rather than centred. The app draws edge to edge, which
        // turns off the system's resize-for-keyboard behaviour: a centred dialog
        // keeps its place and the keyboard covers Save (measured on the
        // emulator). From the top, the form stays clear of it.
        dialog.window?.apply {
            setSoftInputMode(android.view.WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
            attributes = attributes.apply {
                gravity = android.view.Gravity.TOP
                y = dp(activity, 48f)
            }
        }
        dialog.show()
    }
}
