package dev.gpui.mobile;

import android.content.Context;
import android.graphics.Rect;
import android.os.Build;
import android.text.InputType;
import android.util.SparseArray;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewStructure;
import android.view.autofill.AutofillManager;
import android.view.autofill.AutofillValue;

/**
 * A SurfaceView that exposes GPUI's text fields to Android Autofill as virtual children.
 *
 * <p>Use it as the host's GPUI surface and forward {@code gpuiAutofillFields} to
 * {@link #setFields}. The host's invisible IME proxy must be excluded from autofill
 * ({@code setImportantForAutofill(IMPORTANT_FOR_AUTOFILL_NO)}), or it competes with
 * these fields for the session. See {@code src/android/autofill.rs}.</p>
 */
public class GpuiAutofillSurfaceView extends SurfaceView {
    private static final String[] NO_STRINGS = {};
    private static final int[] NO_INTS = {};
    private static final int INTS_PER_FIELD = 5;
    private static final int STRINGS_PER_FIELD = 3;

    private int[] ints = NO_INTS;
    private String[] strings = NO_STRINGS;
    private int focused;

    public GpuiAutofillSurfaceView(Context context) {
        super(context);
        if (Build.VERSION.SDK_INT >= 26) {
            setImportantForAutofill(IMPORTANT_FOR_AUTOFILL_YES);
        }
    }

    /**
     * Replace the fields, on the UI thread: {@code ints} is (id, left, top, right, bottom)
     * and {@code strings} (hint, label, value or null) per field; {@code focusedId} is 0
     * when no field has focus.
     */
    public void setFields(int[] ints, String[] strings, int focusedId) {
        if (Build.VERSION.SDK_INT < 26) return;
        AutofillManager manager = getContext().getSystemService(AutofillManager.class);
        int[] previousInts = this.ints;
        String[] previousStrings = this.strings;
        this.ints = ints;
        this.strings = strings;
        if (manager == null) {
            focused = focusedId;
            return;
        }
        for (int i = 0; i < count(); i++) {
            String value = strings[i * STRINGS_PER_FIELD + 2];
            int id = ints[i * INTS_PER_FIELD];
            if (value != null && !value.equals(previousValue(previousInts, previousStrings, id))) {
                manager.notifyValueChanged(this, id, AutofillValue.forText(value));
            }
        }
        if (focusedId == focused) return;
        if (focused != 0) manager.notifyViewExited(this, focused);
        focused = focusedId;
        Rect bounds = focusedId == 0 ? null : screenBounds(focusedId);
        if (bounds != null) manager.notifyViewEntered(this, focusedId, bounds);
    }

    private int count() {
        return ints.length / INTS_PER_FIELD;
    }

    private static String previousValue(int[] ints, String[] strings, int id) {
        for (int i = 0; i * INTS_PER_FIELD < ints.length; i++) {
            if (ints[i * INTS_PER_FIELD] == id) return strings[i * STRINGS_PER_FIELD + 2];
        }
        return null;
    }

    private Rect screenBounds(int id) {
        for (int i = 0; i < count(); i++) {
            int at = i * INTS_PER_FIELD;
            if (ints[at] == id) {
                int[] origin = new int[2];
                getLocationOnScreen(origin);
                return new Rect(origin[0] + ints[at + 1], origin[1] + ints[at + 2],
                        origin[0] + ints[at + 3], origin[1] + ints[at + 4]);
            }
        }
        return null;
    }

    @Override
    public void onProvideAutofillVirtualStructure(ViewStructure structure, int flags) {
        structure.setClassName(getClass().getName());
        int first = structure.addChildCount(count());
        for (int i = 0; i < count(); i++) {
            int at = i * INTS_PER_FIELD;
            int text = i * STRINGS_PER_FIELD;
            String hint = strings[text];
            boolean secret = "password".equals(hint);
            ViewStructure child = structure.newChild(first + i);
            child.setAutofillId(structure.getAutofillId(), ints[at]);
            child.setClassName("android.widget.EditText");
            child.setAutofillType(View.AUTOFILL_TYPE_TEXT);
            if (!hint.isEmpty()) child.setAutofillHints(new String[] {hint});
            child.setHint(strings[text + 1]);
            if (strings[text + 2] != null) {
                child.setAutofillValue(AutofillValue.forText(strings[text + 2]));
            }
            child.setInputType(secret
                    ? InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD
                    : InputType.TYPE_CLASS_TEXT);
            child.setDataIsSensitive(secret);
            child.setDimens(ints[at + 1], ints[at + 2], 0, 0,
                    ints[at + 3] - ints[at + 1], ints[at + 4] - ints[at + 2]);
            child.setVisibility(VISIBLE);
            child.setEnabled(true);
            child.setFocusable(true);
            child.setFocused(ints[at] == focused);
        }
    }

    @Override
    public void autofill(SparseArray<AutofillValue> values) {
        int[] ids = new int[values.size()];
        String[] texts = new String[values.size()];
        int filled = 0;
        for (int i = 0; i < values.size(); i++) {
            AutofillValue value = values.valueAt(i);
            if (value != null && value.isText()) {
                ids[filled] = values.keyAt(i);
                texts[filled++] = value.getTextValue().toString();
            }
        }
        if (filled == 0) return;
        nativeFill(java.util.Arrays.copyOf(ids, filled), java.util.Arrays.copyOf(texts, filled));
    }

    private static native void nativeFill(int[] ids, String[] values);
}
