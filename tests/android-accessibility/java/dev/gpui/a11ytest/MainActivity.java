package dev.gpui.a11ytest;

import android.app.Activity;
import android.os.Bundle;
import android.view.View;
import android.view.accessibility.AccessibilityManager;
import android.widget.TextView;

public final class MainActivity extends Activity {
    static { System.loadLibrary("a11y_regression"); }
    private static native void test(View view, View.AccessibilityDelegate expected);

    private static native void install(View view);
    private static native void dropAdapter();

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        TextView view = new TextView(this);
        setContentView(view);
        view.post(() -> {
            AccessibilityManager manager = (AccessibilityManager)getSystemService(ACCESSIBILITY_SERVICE);
            if (manager.isEnabled()) throw new AssertionError("Disable accessibility before running");
            test(view, null);
            View.AccessibilityDelegate foreign = new View.AccessibilityDelegate();
            view.setAccessibilityDelegate(foreign);
            test(view, foreign);
            view.setAccessibilityDelegate(null);
            test(view, null);
            install(view);
            view.post(() -> {
                if (view.getAccessibilityNodeProvider() == null) throw new AssertionError("No provider");
                if (view.getAccessibilityNodeProvider().createAccessibilityNodeInfo(-1) == null)
                    throw new AssertionError("No initial tree");
                dropAdapter();
                view.post(() -> {
                    test(view, null);
                    install(view);
                    view.post(() -> {
                        view.setAccessibilityDelegate(foreign);
                        dropAdapter();
                        view.post(() -> {
                            test(view, foreign);
                            view.setText("PASS: delegate lifecycle and disabled event delivery");
                            android.util.Log.i("GpuiA11yRegression", "PASS API " + android.os.Build.VERSION.SDK_INT);
                        });
                    });
                });
            });
        });
    }
}
