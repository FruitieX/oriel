package org.fruitiex.oriel;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.media.projection.MediaProjectionManager;
import android.provider.Settings;
import android.graphics.Insets;
import android.os.Build;
import android.os.Bundle;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.FrameLayout;

/**
 * The Rust app runs in this NativeActivity (android_main). Java is only
 * here for what the NDK lacks: the soft keyboard's text and the clipboard.
 */
public class OrielActivity extends NativeActivity {
    static {
        // Also makes the native methods below resolvable.
        System.loadLibrary("oriel_android");
    }

    /** Typed text, in UTF-16 code units as the keyboard produced them. */
    static native void nativeText(String text);
    /** Delete `before` characters before the cursor and `after` after it. */
    static native void nativeDelete(int before, int after);
    /** A key the keyboard sent as a key (Enter, arrows, Backspace...). */
    static native void nativeKey(int keyCode, boolean down, int metaState);
    /** The keyboard was shown or hidden. */
    static native void nativeKeyboardShown(boolean shown);
    /** Window areas under system bars and cut-outs, and the keyboard height (px). */
    static native void nativeInsets(int left, int top, int right, int bottom, int keyboard);
    /** Sharing this phone (ShareService): the shared size, frames, the end. */
    static native void nativeShareStarted(int width, int height);
    static native void nativeShareStopped();
    static native void nativeEncodedFrame(byte[] data, boolean keyframe, int encodeUs);

    private static final int REQUEST_SHARE = 7;
    private boolean askedAccessibility;

    private TextSink sink;
    private boolean keyboardShown;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        sink = new TextSink(this);
        addContentView(sink, new FrameLayout.LayoutParams(1, 1));
        // Edge to edge (forced from Android 15): tell Rust what is covered.
        getWindow().getDecorView().setOnApplyWindowInsetsListener(
                new View.OnApplyWindowInsetsListener() {
                    public WindowInsets onApplyWindowInsets(View v, WindowInsets insets) {
                        boolean shown;
                        if (Build.VERSION.SDK_INT >= 30) {
                            Insets bars = insets.getInsets(WindowInsets.Type.systemBars()
                                    | WindowInsets.Type.displayCutout());
                            Insets ime = insets.getInsets(WindowInsets.Type.ime());
                            nativeInsets(bars.left, bars.top, bars.right, bars.bottom,
                                    ime.bottom);
                            shown = insets.isVisible(WindowInsets.Type.ime());
                        } else {
                            // The bottom inset includes the keyboard when shown.
                            int bottom = insets.getSystemWindowInsetBottom();
                            int stable = insets.getStableInsetBottom();
                            shown = bottom > stable;
                            nativeInsets(insets.getSystemWindowInsetLeft(),
                                    insets.getSystemWindowInsetTop(),
                                    insets.getSystemWindowInsetRight(), stable,
                                    shown ? bottom : 0);
                        }
                        if (shown != keyboardShown) {
                            keyboardShown = shown;
                            nativeKeyboardShown(shown);
                        }
                        return v.onApplyWindowInsets(insets);
                    }
                });
    }

    /**
     * The Rust side runs one event loop per process and cannot start
     * another, so a destroyed activity takes the process with it; the next
     * launch starts fresh.
     */
    @Override
    protected void onDestroy() {
        super.onDestroy();
        System.exit(0);
    }

    // Sharing this phone. Called from Rust.

    public void requestScreenShare() {
        runOnUiThread(new Runnable() {
            public void run() {
                if (ShareService.instance != null) {
                    askForAccessibility();
                    return;
                }
                MediaProjectionManager mpm = getSystemService(MediaProjectionManager.class);
                startActivityForResult(mpm.createScreenCaptureIntent(), REQUEST_SHARE);
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == REQUEST_SHARE && result == RESULT_OK && data != null) {
            startForegroundService(new Intent(this, ShareService.class)
                    .putExtra("code", result).putExtra("data", data));
            askForAccessibility();
        }
    }

    /** Being controlled needs the accessibility service: ask once a run. */
    private void askForAccessibility() {
        if (OrielAccessibilityService.instance == null && !askedAccessibility) {
            askedAccessibility = true;
            startActivity(new Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS));
        }
    }

    public void stopScreenShare() {
        stopService(new Intent(this, ShareService.class));
    }

    public boolean startEncoder(String mime, int width, int height, int fps, int kbps) {
        ShareService s = ShareService.instance;
        return s != null && s.startEncoder(mime, width, height, fps, kbps);
    }

    public void stopEncoder() {
        ShareService s = ShareService.instance;
        if (s != null) {
            s.stopEncoder();
        }
    }

    public void requestKeyframe() {
        ShareService s = ShareService.instance;
        if (s != null) {
            s.requestKeyframe();
        }
    }

    public void setBitrate(int kbps) {
        ShareService s = ShareService.instance;
        if (s != null) {
            s.setBitrate(kbps);
        }
    }

    public boolean a11yEnabled() {
        return OrielAccessibilityService.instance != null;
    }

    /** From the shared (encoded) size to the screen's. */
    private static float sx() {
        return ShareService.shareWidth > 0
                ? ShareService.screenWidth / (float) ShareService.shareWidth : 1f;
    }

    private static float sy() {
        return ShareService.shareHeight > 0
                ? ShareService.screenHeight / (float) ShareService.shareHeight : 1f;
    }

    public void a11yTap(float x, float y, boolean longPress) {
        OrielAccessibilityService a = OrielAccessibilityService.instance;
        if (a != null) {
            a.tap(x * sx(), y * sy(), longPress);
        }
    }

    public void a11ySwipe(float[] points, int ms) {
        OrielAccessibilityService a = OrielAccessibilityService.instance;
        if (a != null) {
            for (int i = 0; i + 1 < points.length; i += 2) {
                points[i] *= sx();
                points[i + 1] *= sy();
            }
            a.swipe(points, ms);
        }
    }

    public void a11yGlobal(int action) {
        OrielAccessibilityService a = OrielAccessibilityService.instance;
        if (a != null) {
            a.global(action);
        }
    }

    public void a11yText(String text) {
        OrielAccessibilityService a = OrielAccessibilityService.instance;
        if (a != null) {
            a.text(text);
        }
    }

    public void a11yBackspace() {
        OrielAccessibilityService a = OrielAccessibilityService.instance;
        if (a != null) {
            a.backspace();
        }
    }

    public void a11yEnter() {
        OrielAccessibilityService a = OrielAccessibilityService.instance;
        if (a != null) {
            a.enter();
        }
    }

    /** Called from Rust: full screen hides the system bars (a swipe shows them). */
    public void setImmersive(final boolean on) {
        runOnUiThread(new Runnable() {
            public void run() {
                View decor = getWindow().getDecorView();
                if (Build.VERSION.SDK_INT >= 30) {
                    WindowInsetsController c = decor.getWindowInsetsController();
                    if (c == null) {
                        return;
                    }
                    if (on) {
                        c.setSystemBarsBehavior(
                                WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                        c.hide(WindowInsets.Type.systemBars());
                    } else {
                        c.show(WindowInsets.Type.systemBars());
                    }
                } else {
                    decor.setSystemUiVisibility(on
                            ? View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                                    | View.SYSTEM_UI_FLAG_FULLSCREEN
                                    | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            : 0);
                }
            }
        });
    }

    /** Called from Rust. */
    public void showKeyboard(final boolean show) {
        runOnUiThread(new Runnable() {
            public void run() {
                InputMethodManager imm =
                        (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
                if (show) {
                    sink.reset();
                    sink.setFocusableInTouchMode(true);
                    sink.requestFocus();
                    imm.restartInput(sink);
                    imm.showSoftInput(sink, 0);
                } else {
                    imm.hideSoftInputFromWindow(sink.getWindowToken(), 0);
                }
            }
        });
    }

    /** Called from Rust: open a web page (signing in to the hub). */
    public void openUrl(final String url) {
        runOnUiThread(new Runnable() {
            public void run() {
                try {
                    startActivity(new android.content.Intent(
                            android.content.Intent.ACTION_VIEW, android.net.Uri.parse(url)));
                } catch (Exception e) {
                    // No browser.
                }
            }
        });
    }

    /** Called from Rust: the name from Settings → About phone, else the model. */
    public String deviceName() {
        String name = android.provider.Settings.Global.getString(
                getContentResolver(), "device_name");
        return name != null && !name.isEmpty() ? name : Build.MODEL;
    }

    /** Called from Rust; null when empty or not readable. */
    public String clipboardText() {
        ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        ClipData clip = cm.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) {
            return null;
        }
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }

    /** Called from Rust. */
    public void setClipboardText(final String text) {
        runOnUiThread(new Runnable() {
            public void run() {
                ClipboardManager cm =
                        (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
                cm.setPrimaryClip(ClipData.newPlainText("oriel", text));
            }
        });
    }

    /**
     * An invisible editor the keyboard types into. What it composes
     * (autocorrect, swipe typing) is passed on as the difference to what was
     * already sent, so the host sees each change as it happens.
     */
    static class TextSink extends View {
        private String composing = "";

        TextSink(Context context) {
            super(context);
            setFocusable(true);
        }

        void reset() {
            composing = "";
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE;
            info.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN
                    | EditorInfo.IME_FLAG_NO_EXTRACT_UI
                    | EditorInfo.IME_ACTION_NONE;
            return new BaseInputConnection(this, false) {
                @Override
                public boolean commitText(CharSequence text, int newCursorPosition) {
                    replaceComposing(text.toString());
                    composing = "";
                    return true;
                }

                @Override
                public boolean setComposingText(CharSequence text, int newCursorPosition) {
                    replaceComposing(text.toString());
                    return true;
                }

                @Override
                public boolean finishComposingText() {
                    composing = "";
                    return true;
                }

                @Override
                public boolean deleteSurroundingText(int before, int after) {
                    nativeDelete(before, after);
                    return true;
                }

                @Override
                public boolean sendKeyEvent(KeyEvent event) {
                    if (event.getAction() == KeyEvent.ACTION_DOWN
                            || event.getAction() == KeyEvent.ACTION_UP) {
                        nativeKey(event.getKeyCode(),
                                event.getAction() == KeyEvent.ACTION_DOWN,
                                event.getMetaState());
                    }
                    return true;
                }
            };
        }

        private void replaceComposing(String text) {
            int common = 0;
            int max = Math.min(composing.length(), text.length());
            while (common < max && composing.charAt(common) == text.charAt(common)) {
                common++;
            }
            int remove = composing.codePointCount(common, composing.length());
            if (remove > 0) {
                nativeDelete(remove, 0);
            }
            if (common < text.length()) {
                nativeText(text.substring(common));
            }
            composing = text;
        }
    }
}
