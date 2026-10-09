package dev.gpui.mobile;

import android.os.Bundle;

import androidx.annotation.NonNull;
import androidx.biometric.BiometricPrompt;
import androidx.core.content.ContextCompat;
import androidx.fragment.app.FragmentActivity;

/**
 * Transparent helper Activity that displays a BiometricPrompt.
 *
 * <p>NativeActivity is not a FragmentActivity, so it cannot host a
 * BiometricPrompt directly. This lightweight Activity is launched by
 * {@link GpuiLocalAuth#authenticate} and immediately shows the prompt.
 * On completion (success, failure, or cancellation) it hands the result to the
 * thread waiting in {@link GpuiLocalAuth#authenticate} and finishes; if it goes
 * away first, the wait ends as cancelled.</p>
 */
public class GpuiAuthActivity extends FragmentActivity {

    private GpuiLocalAuth.Request mRequest;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        mRequest = GpuiLocalAuth.pending();
        if (mRequest == null) {
            // Recreated after process death: nobody is waiting any more.
            finish();
            return;
        }
        String reason = mRequest.reason;
        if (reason == null || reason.isEmpty()) {
            reason = "Verify your identity";
        }

        BiometricPrompt.PromptInfo promptInfo = new BiometricPrompt.PromptInfo.Builder()
            .setTitle("Authentication Required")
            .setSubtitle(reason)
            .setNegativeButtonText("Cancel")
            .build();

        BiometricPrompt biometricPrompt = new BiometricPrompt(this,
            ContextCompat.getMainExecutor(this),
            new BiometricPrompt.AuthenticationCallback() {
                @Override
                public void onAuthenticationSucceeded(@NonNull BiometricPrompt.AuthenticationResult result) {
                    super.onAuthenticationSucceeded(result);
                    deliverResult(0); // success
                    finish();
                }

                @Override
                public void onAuthenticationFailed() {
                    super.onAuthenticationFailed();
                    // Called on each failed attempt; don't finish yet.
                    // The system will either allow retry or call onAuthenticationError.
                }

                @Override
                public void onAuthenticationError(int errorCode, @NonNull CharSequence errString) {
                    super.onAuthenticationError(errorCode, errString);
                    int result;
                    switch (errorCode) {
                        case BiometricPrompt.ERROR_USER_CANCELED:
                        case BiometricPrompt.ERROR_NEGATIVE_BUTTON:
                        case BiometricPrompt.ERROR_CANCELED: // e.g. the app went to the background
                            result = 4; // cancelled
                            break;
                        case BiometricPrompt.ERROR_LOCKOUT:
                        case BiometricPrompt.ERROR_LOCKOUT_PERMANENT:
                            result = 6; // lockout
                            break;
                        case BiometricPrompt.ERROR_NO_BIOMETRICS:
                            result = 3; // not enrolled
                            break;
                        case BiometricPrompt.ERROR_HW_NOT_PRESENT:
                        case BiometricPrompt.ERROR_HW_UNAVAILABLE:
                            result = 2; // not available
                            break;
                        case BiometricPrompt.ERROR_NO_DEVICE_CREDENTIAL:
                            result = 5; // passcode not set
                            break;
                        default:
                            result = 7; // other
                            break;
                    }
                    deliverResult(result);
                    finish();
                }
            });

        // After a configuration change the prompt is still showing; the new
        // BiometricPrompt above only reconnects its callback.
        if (savedInstanceState == null) biometricPrompt.authenticate(promptInfo);
    }

    @Override
    protected void onDestroy() {
        // Gone without a result, e.g. the task brought to front from the launcher.
        if (isFinishing()) deliverResult(4); // cancelled
        super.onDestroy();
    }

    private void deliverResult(int result) {
        if (mRequest != null) mRequest.complete(result);
    }
}
