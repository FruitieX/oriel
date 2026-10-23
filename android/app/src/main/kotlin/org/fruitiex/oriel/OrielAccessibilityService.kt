package org.fruitiex.oriel

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

/**
 * Controls the phone while it is shared: taps, swipes and Back/Home/Recents
 * as gestures and global actions, typing into the focused field. Only acts
 * on what the Rust host asks (input from paired devices).
 */
class OrielAccessibilityService : AccessibilityService() {
    override fun onServiceConnected() {
        instance = this
    }

    override fun onUnbind(intent: Intent?): Boolean {
        instance = null
        return super.onUnbind(intent)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    private fun dispatch(path: Path, ms: Long) {
        val stroke = GestureDescription.StrokeDescription(path, 0, maxOf(1L, ms))
        dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), null, null)
    }

    fun tap(x: Float, y: Float, longPress: Boolean) {
        dispatch(Path().apply { moveTo(x, y) }, if (longPress) 700 else 40)
    }

    fun swipe(points: FloatArray, ms: Int) {
        if (points.size < 2) return
        val path = Path().apply {
            moveTo(points[0], points[1])
            for (i in 2 until points.size - 1 step 2) lineTo(points[i], points[i + 1])
            if (points.size == 2) lineTo(points[0] + 1, points[1])
        }
        dispatch(path, ms.toLong())
    }

    fun global(action: Int) {
        performGlobalAction(
            when (action) {
                1 -> GLOBAL_ACTION_BACK
                2 -> GLOBAL_ACTION_HOME
                else -> GLOBAL_ACTION_RECENTS
            },
        )
    }

    private fun focused(): AccessibilityNodeInfo? =
        findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { it.isEditable }

    /** Replace the selection (or `deleteBefore` characters before the cursor) with `insert`. */
    private fun edit(insert: String, deleteBefore: Int) {
        val node = focused() ?: return
        val text = if (Build.VERSION.SDK_INT >= 26 && node.isShowingHintText) "" else node.text?.toString().orEmpty()
        var start = node.textSelectionStart
        var end = node.textSelectionEnd
        if (start < 0 || end < 0 || start > text.length || end > text.length) {
            start = text.length
            end = text.length
        }
        if (start == end) start = maxOf(0, start - deleteBefore)
        val next = text.substring(0, start) + insert + text.substring(end)
        node.performAction(
            AccessibilityNodeInfo.ACTION_SET_TEXT,
            Bundle().apply {
                putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next)
            },
        )
        val cursor = start + insert.length
        node.performAction(
            AccessibilityNodeInfo.ACTION_SET_SELECTION,
            Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, cursor)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, cursor)
            },
        )
    }

    fun text(s: String) = edit(s, 0)

    fun backspace() = edit("", 1)

    fun enter() {
        val node = focused() ?: return
        if (Build.VERSION.SDK_INT >= 30) {
            node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
        }
    }

    companion object {
        @Volatile var instance: OrielAccessibilityService? = null
    }
}
