package org.fruitiex.oriel;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.content.Intent;
import android.graphics.Path;
import android.os.Build;
import android.os.Bundle;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;

/**
 * Controls the phone while it is shared: taps, swipes and Back/Home/Recents
 * as gestures and global actions, typing into the focused field. Only acts
 * on what the Rust host asks (input from paired devices).
 */
public class OrielAccessibilityService extends AccessibilityService {
    static volatile OrielAccessibilityService instance;

    @Override
    protected void onServiceConnected() {
        instance = this;
    }

    @Override
    public boolean onUnbind(Intent intent) {
        instance = null;
        return super.onUnbind(intent);
    }

    @Override
    public void onAccessibilityEvent(AccessibilityEvent event) {}

    @Override
    public void onInterrupt() {}

    private void dispatch(Path path, long ms) {
        GestureDescription.StrokeDescription stroke =
                new GestureDescription.StrokeDescription(path, 0, Math.max(1, ms));
        dispatchGesture(new GestureDescription.Builder().addStroke(stroke).build(), null, null);
    }

    void tap(float x, float y, boolean longPress) {
        Path p = new Path();
        p.moveTo(x, y);
        dispatch(p, longPress ? 700 : 40);
    }

    void swipe(float[] points, int ms) {
        if (points.length < 2) {
            return;
        }
        Path p = new Path();
        p.moveTo(points[0], points[1]);
        for (int i = 2; i + 1 < points.length; i += 2) {
            p.lineTo(points[i], points[i + 1]);
        }
        if (points.length == 2) {
            p.lineTo(points[0] + 1, points[1]);
        }
        dispatch(p, ms);
    }

    void global(int action) {
        performGlobalAction(action == 1 ? GLOBAL_ACTION_BACK
                : action == 2 ? GLOBAL_ACTION_HOME : GLOBAL_ACTION_RECENTS);
    }

    private AccessibilityNodeInfo focused() {
        AccessibilityNodeInfo node = findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        return node != null && node.isEditable() ? node : null;
    }

    /** Replace [start, end) of the focused field's text with `insert`. */
    private void edit(String insert, int deleteBefore) {
        AccessibilityNodeInfo node = focused();
        if (node == null) {
            return;
        }
        CharSequence current = node.getText();
        if (current == null || (Build.VERSION.SDK_INT >= 26 && node.isShowingHintText())) {
            current = "";
        }
        String text = current.toString();
        int start = node.getTextSelectionStart();
        int end = node.getTextSelectionEnd();
        if (start < 0 || end < 0 || start > text.length() || end > text.length()) {
            start = end = text.length();
        }
        if (start == end) {
            start = Math.max(0, start - deleteBefore);
        }
        String next = text.substring(0, start) + insert + text.substring(end);
        Bundle args = new Bundle();
        args.putCharSequence(
                AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next);
        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args);
        int cursor = start + insert.length();
        Bundle sel = new Bundle();
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, cursor);
        sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, cursor);
        node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel);
    }

    void text(String s) {
        edit(s, 0);
    }

    void backspace() {
        edit("", 1);
    }

    void enter() {
        AccessibilityNodeInfo node = focused();
        if (node != null && Build.VERSION.SDK_INT >= 30) {
            node.performAction(
                    AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.getId());
        }
    }
}
