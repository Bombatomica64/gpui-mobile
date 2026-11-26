package dev.gpui.mobile;

import android.graphics.Rect;
import android.os.Build;
import android.text.InputType;
import android.util.SparseArray;
import android.view.View;
import android.view.ViewStructure;
import android.view.autofill.AutofillManager;
import android.view.autofill.AutofillValue;

/**
 * Exposes GPUI's text fields to Android Autofill as virtual children of the host's IME
 * proxy, the view the keyboard is connected to.
 *
 * <p>Not the surface GPUI draws to: Android hands a fill request to the autofill service
 * only once the keyboard's inline suggestion request for the same field has arrived, and
 * it matches the two by view id (as for a WebView, the keyboard knows no virtual ids).
 * With the fields on another view the request is dropped, and a password manager neither
 * offers a login nor asks to save one.</p>
 *
 * <p>In the proxy: mark it {@code IMPORTANT_FOR_AUTOFILL_YES}, return
 * {@code AUTOFILL_TYPE_NONE} from {@code getAutofillType()} (the proxy itself holds nothing
 * to fill), and forward {@code onProvideAutofillVirtualStructure} to
 * {@link #provideStructure} and {@code autofill(SparseArray)} to {@link #fill}. Forward the
 * Activity's {@code gpuiAutofillFields} to {@link #setFields}. See
 * {@code src/android/autofill.rs}.</p>
 */
public final class GpuiAutofill {
    private static final String[] NO_STRINGS = {};
    private static final int[] NO_INTS = {};
    private static final int INTS_PER_FIELD = 5;
    private static final int STRINGS_PER_FIELD = 3;

    private final View host;
    private final View surface;
    private int[] ints = NO_INTS;
    private String[] strings = NO_STRINGS;
    private int focused;

    /** {@code host} is the IME proxy; field bounds are in pixels of {@code surface}. */
    public GpuiAutofill(View host, View surface) {
        this.host = host;
        this.surface = surface;
    }

    /**
     * Replace the fields, on the UI thread: {@code ints} is (id, left, top, right, bottom)
     * and {@code strings} (hint, label, value or null) per field; {@code focusedId} is 0
     * when no field has focus. An empty list after a password was typed ends the form:
     * the session is committed, and Android offers to save the login.
     */
    public void setFields(int[] ints, String[] strings, int focusedId) {
        if (Build.VERSION.SDK_INT < 26) return;
        AutofillManager manager = host.getContext().getSystemService(AutofillManager.class);
        int[] previousInts = this.ints;
        String[] previousStrings = this.strings;
        this.ints = ints;
        this.strings = strings;
        if (manager == null) {
            focused = focusedId;
            return;
        }
        if (count() == 0 && typedPassword(previousInts, previousStrings)) {
            manager.commit();
            focused = 0;
            return;
        }
        for (int i = 0; i < count(); i++) {
            String value = strings[i * STRINGS_PER_FIELD + 2];
            int id = ints[i * INTS_PER_FIELD];
            if (value != null && !value.equals(previousValue(previousInts, previousStrings, id))) {
                manager.notifyValueChanged(host, id, AutofillValue.forText(value));
            }
        }
        if (focusedId == focused) return;
        if (focused != 0) manager.notifyViewExited(host, focused);
        focused = focusedId;
        Rect bounds = focusedId == 0 ? null : screenBounds(focusedId);
        if (bounds != null) manager.notifyViewEntered(host, focusedId, bounds);
    }

    /** The proxy's {@code onProvideAutofillVirtualStructure}. */
    public void provideStructure(ViewStructure structure) {
        // The proxy is an invisible 1x1 view; a password manager that sees that ignores
        // its children. Describe it as the visible surface instead, so the fields, placed
        // relative to it, sit where GPUI draws them.
        int[] hostOrigin = new int[2];
        int[] surfaceOrigin = new int[2];
        host.getLocationOnScreen(hostOrigin);
        surface.getLocationOnScreen(surfaceOrigin);
        structure.setDimens(host.getLeft() + surfaceOrigin[0] - hostOrigin[0],
                host.getTop() + surfaceOrigin[1] - hostOrigin[1], 0, 0,
                surface.getWidth(), surface.getHeight());
        if (Build.VERSION.SDK_INT >= 28) structure.setAlpha(1f);
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
            // Fields scrolled out of the surface are still in GPUI's tree; a password
            // manager should only weigh the form on screen.
            boolean onScreen = ints[at + 3] > 0 && ints[at + 4] > 0
                    && ints[at + 1] < surface.getWidth() && ints[at + 2] < surface.getHeight();
            child.setVisibility(onScreen ? View.VISIBLE : View.INVISIBLE);
            child.setEnabled(true);
            child.setFocusable(true);
            child.setFocused(ints[at] == focused);
        }
    }

    /** The proxy's {@code autofill(SparseArray)}: the dataset the user picked. */
    public void fill(SparseArray<AutofillValue> values) {
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

    private int count() {
        return ints.length / INTS_PER_FIELD;
    }

    private static boolean typedPassword(int[] ints, String[] strings) {
        for (int i = 0; i * INTS_PER_FIELD < ints.length; i++) {
            int text = i * STRINGS_PER_FIELD;
            if ("password".equals(strings[text]) && strings[text + 2] != null
                    && !strings[text + 2].isEmpty()) {
                return true;
            }
        }
        return false;
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
                surface.getLocationOnScreen(origin);
                return new Rect(origin[0] + ints[at + 1], origin[1] + ints[at + 2],
                        origin[0] + ints[at + 3], origin[1] + ints[at + 4]);
            }
        }
        return null;
    }

    private static native void nativeFill(int[] ids, String[] values);
}
