package org.fruitiex.oriel

import android.app.NativeActivity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout

/**
 * The Rust app runs in this NativeActivity (android_main). Kotlin is here
 * for what the NDK lacks: the soft keyboard's text, insets, the clipboard,
 * sharing the screen and being controlled. Rust calls the public methods
 * over JNI; their signatures are part of that contract.
 */
class OrielActivity : NativeActivity() {
    private lateinit var sink: TextSink
    private var keyboardShown = false
    private var askedAccessibility = false
    /** Started by a wake-up: share, then step aside. */
    private var woken = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        sink = TextSink(this)
        addContentView(sink, FrameLayout.LayoutParams(1, 1))
        // Edge to edge (forced from Android 15): tell Rust what is covered.
        window.decorView.setOnApplyWindowInsetsListener { v, insets ->
            val shown: Boolean
            if (Build.VERSION.SDK_INT >= 30) {
                val bars = insets.getInsets(
                    WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout(),
                )
                val ime = insets.getInsets(WindowInsets.Type.ime())
                nativeInsets(bars.left, bars.top, bars.right, bars.bottom, ime.bottom)
                shown = insets.isVisible(WindowInsets.Type.ime())
            } else {
                // The bottom inset includes the keyboard when shown.
                @Suppress("DEPRECATION")
                val bottom = insets.systemWindowInsetBottom
                @Suppress("DEPRECATION")
                val stable = insets.stableInsetBottom
                shown = bottom > stable
                @Suppress("DEPRECATION")
                nativeInsets(
                    insets.systemWindowInsetLeft, insets.systemWindowInsetTop,
                    insets.systemWindowInsetRight, stable, if (shown) bottom else 0,
                )
            }
            if (shown != keyboardShown) {
                keyboardShown = shown
                nativeKeyboardShown(shown)
            }
            v.onApplyWindowInsets(insets)
        }
        Push.register(this)
        handle(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handle(intent)
    }

    /** A wake-up, or its notification, asks to share. */
    private fun handle(intent: Intent?) {
        when (intent?.action) {
            ACTION_SHARE -> requestScreenShare()
            ACTION_WAKE -> {
                woken = true
                requestScreenShare()
            }
        }
    }

    /**
     * The Rust side runs one event loop per process and cannot start
     * another, so a destroyed activity takes the process with it; the next
     * launch starts fresh.
     */
    override fun onDestroy() {
        super.onDestroy()
        System.exit(0)
    }

    // Sharing this phone.

    fun requestScreenShare() = runOnUiThread {
        if (ShareService.instance != null) {
            shareStarted()
            return@runOnUiThread
        }
        val mpm = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        startActivityForResult(mpm.createScreenCaptureIntent(), REQUEST_SHARE)
    }

    @Deprecated("NativeActivity has no activity result API")
    override fun onActivityResult(request: Int, result: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(request, result, data)
        if (request == REQUEST_SHARE && result == RESULT_OK && data != null) {
            startForegroundService(
                Intent(this, ShareService::class.java)
                    .putExtra("code", result)
                    .putExtra("data", data),
            )
            shareStarted()
        } else if (request == REQUEST_SHARE && woken) {
            woken = false
            moveTaskToBack(true)
        }
    }

    private fun shareStarted() {
        if (woken) {
            // Leave the phone as it was, for whoever connects.
            woken = false
            moveTaskToBack(true)
        } else {
            askForAccessibility()
        }
    }

    /** Being controlled needs the accessibility service: ask once a run. */
    private fun askForAccessibility() {
        if (OrielAccessibilityService.instance == null && !askedAccessibility) {
            askedAccessibility = true
            startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
        }
    }

    fun stopScreenShare() {
        stopService(Intent(this, ShareService::class.java))
    }

    fun startEncoder(mime: String, width: Int, height: Int, fps: Int, kbps: Int): Boolean =
        ShareService.instance?.startEncoder(mime, width, height, fps, kbps) ?: false

    fun stopEncoder() {
        ShareService.instance?.stopEncoder()
    }

    fun requestKeyframe() {
        ShareService.instance?.requestKeyframe()
    }

    fun setBitrate(kbps: Int) {
        ShareService.instance?.setBitrate(kbps)
    }

    // Being controlled (coordinates in the shared size, scaled to the screen).

    fun a11yEnabled(): Boolean = OrielAccessibilityService.instance != null

    fun a11yTap(x: Float, y: Float, longPress: Boolean) {
        OrielAccessibilityService.instance?.tap(x * ShareService.scaleX, y * ShareService.scaleY, longPress)
    }

    fun a11ySwipe(points: FloatArray, ms: Int) {
        val a = OrielAccessibilityService.instance ?: return
        for (i in 0 until points.size - 1 step 2) {
            points[i] *= ShareService.scaleX
            points[i + 1] *= ShareService.scaleY
        }
        a.swipe(points, ms)
    }

    fun a11yGlobal(action: Int) {
        OrielAccessibilityService.instance?.global(action)
    }

    fun a11yText(text: String) {
        OrielAccessibilityService.instance?.text(text)
    }

    fun a11yBackspace() {
        OrielAccessibilityService.instance?.backspace()
    }

    fun a11yEnter() {
        OrielAccessibilityService.instance?.enter()
    }

    // The rest of the app.

    /** Full screen hides the system bars (a swipe shows them). */
    fun setFullScreen(on: Boolean) = runOnUiThread {
        val decor = window.decorView
        if (Build.VERSION.SDK_INT >= 30) {
            val c = decor.windowInsetsController ?: return@runOnUiThread
            if (on) {
                c.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                c.hide(WindowInsets.Type.systemBars())
            } else {
                c.show(WindowInsets.Type.systemBars())
            }
        } else {
            @Suppress("DEPRECATION")
            decor.systemUiVisibility = if (on) {
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or View.SYSTEM_UI_FLAG_FULLSCREEN or
                    View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
            } else {
                0
            }
        }
    }

    fun showKeyboard(show: Boolean) = runOnUiThread {
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        if (show) {
            sink.reset()
            sink.isFocusableInTouchMode = true
            sink.requestFocus()
            imm.restartInput(sink)
            imm.showSoftInput(sink, 0)
        } else {
            imm.hideSoftInputFromWindow(sink.windowToken, 0)
        }
    }

    /** Open a web page (signing in to the hub). */
    fun openUrl(url: String) = runOnUiThread {
        try {
            startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
        } catch (_: Exception) {
            // No browser.
        }
    }

    /** The name from Settings → About phone, else the model. */
    fun deviceName(): String =
        Settings.Global.getString(contentResolver, "device_name")?.takeIf { it.isNotEmpty() }
            ?: Build.MODEL

    /** Null when empty or not readable. */
    fun clipboardText(): String? {
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val clip = cm.primaryClip ?: return null
        if (clip.itemCount == 0) return null
        return clip.getItemAt(0).coerceToText(this)?.toString()
    }

    fun setClipboardText(text: String) = runOnUiThread {
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        cm.setPrimaryClip(ClipData.newPlainText("oriel", text))
    }

    /**
     * An invisible editor the keyboard types into. What it composes
     * (autocorrect, swipe typing) is passed on as the difference to what
     * was already sent, so the host sees each change as it happens.
     */
    private class TextSink(context: Context) : View(context) {
        private var composing = ""

        init {
            isFocusable = true
        }

        fun reset() {
            composing = ""
        }

        override fun onCheckIsTextEditor() = true

        override fun onCreateInputConnection(info: EditorInfo): InputConnection {
            info.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            info.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or
                EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_ACTION_NONE
            return object : BaseInputConnection(this, false) {
                override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
                    replaceComposing(text.toString())
                    composing = ""
                    return true
                }

                override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
                    replaceComposing(text.toString())
                    return true
                }

                override fun finishComposingText(): Boolean {
                    composing = ""
                    return true
                }

                override fun deleteSurroundingText(before: Int, after: Int): Boolean {
                    nativeDelete(before, after)
                    return true
                }

                override fun sendKeyEvent(event: KeyEvent): Boolean {
                    if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
                        nativeKey(event.keyCode, event.action == KeyEvent.ACTION_DOWN, event.metaState)
                    }
                    return true
                }
            }
        }

        private fun replaceComposing(text: String) {
            var common = 0
            val max = minOf(composing.length, text.length)
            while (common < max && composing[common] == text[common]) common++
            val remove = composing.codePointCount(common, composing.length)
            if (remove > 0) nativeDelete(remove, 0)
            if (common < text.length) nativeText(text.substring(common))
            composing = text
        }
    }

    companion object {
        /** A wake-up notification's action: start sharing. */
        const val ACTION_SHARE = "org.fruitiex.oriel.SHARE"
        /** A wake-up allowed to start sharing unattended (see `Wake`). */
        const val ACTION_WAKE = "org.fruitiex.oriel.WAKE_SHARE"
        private const val REQUEST_SHARE = 7

        init {
            // Also makes the native methods below resolvable.
            System.loadLibrary("oriel_android")
        }

        /** Typed text, in UTF-16 code units as the keyboard produced them. */
        @JvmStatic external fun nativeText(text: String)

        /** Delete `before` characters before the cursor and `after` after it. */
        @JvmStatic external fun nativeDelete(before: Int, after: Int)

        /** A key the keyboard sent as a key (Enter, arrows, Backspace...). */
        @JvmStatic external fun nativeKey(keyCode: Int, down: Boolean, metaState: Int)

        @JvmStatic external fun nativeKeyboardShown(shown: Boolean)

        /** Window areas under system bars and cut-outs, and the keyboard height (px). */
        @JvmStatic external fun nativeInsets(left: Int, top: Int, right: Int, bottom: Int, keyboard: Int)

        /** Sharing this phone (ShareService): the shared size, frames, the end. */
        @JvmStatic external fun nativeShareStarted(width: Int, height: Int)

        @JvmStatic external fun nativeShareStopped()

        @JvmStatic external fun nativeEncodedFrame(data: ByteArray, keyframe: Boolean, encodeUs: Int)

        /** The Firebase token the hub wakes this phone with. */
        @JvmStatic external fun nativePushToken(token: String)
    }
}
