package net.pedrosoares.mobilecoder

import android.app.Activity
import android.app.AlertDialog
import android.graphics.Color
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.WindowManager
import android.widget.EditText
import android.widget.LinearLayout

/**
 * One line of text, asked for in a native dialog.
 *
 * Every field in the Git pane - the token, a repository name, a commit message
 * - comes through here, because Freya receives no on-screen keyboard text
 * inside a NativeActivity. The Rust side says what it wants and why (see
 * `mc_ui::prompt`); this only draws it.
 */
object TextPromptDialog {

    fun show(
        activity: Activity,
        title: String,
        hint: String,
        secret: Boolean,
        multiline: Boolean,
        onText: (String) -> Unit,
        onClosed: () -> Unit,
    ) {
        val input = EditText(activity).apply {
            setHint(hint)
            setTextColor(Color.rgb(223, 232, 234))
            setHintTextColor(Color.rgb(100, 112, 117))
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            inputType = when {
                secret -> InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
                multiline ->
                    InputType.TYPE_CLASS_TEXT or
                        InputType.TYPE_TEXT_FLAG_MULTI_LINE or
                        InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
                // A token or a repository name is not prose: no autocorrect.
                else -> InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            }
            maxLines = if (multiline) 4 else 1
        }

        val padding = TypedValue.applyDimension(
            TypedValue.COMPLEX_UNIT_DIP, 24f, activity.resources.displayMetrics,
        ).toInt()
        val layout = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(padding, padding / 2, padding, 0)
            addView(input)
        }

        val dialog = AlertDialog.Builder(activity)
            .setTitle(title)
            .setView(layout)
            .setPositiveButton("OK") { _, _ -> onText(input.text.toString()) }
            .setNegativeButton("Cancel", null)
            .setOnDismissListener { onClosed() }
            .create()

        // Top-aligned, for the same reason as the settings form: the app draws
        // edge to edge, so the system does not resize windows for the keyboard
        // and a centred dialog ends up behind it.
        dialog.window?.apply {
            setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
            attributes = attributes.apply {
                gravity = Gravity.TOP
                y = padding * 2
            }
        }
        dialog.show()
        input.requestFocus()
    }
}
