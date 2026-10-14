#!/usr/bin/env bash
# Requires cargo-ndk, Android SDK/build-tools 34, JDK, a debug keystore, and
# an arm64 emulator with accessibility disabled. Usage: ./run.sh [adb-serial]
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
TOOLS="$SDK/build-tools/34.0.0"
ANDROID_JAR="$SDK/platforms/android-34/android.jar"
SERIAL="${1:-emulator-5554}"
export ANDROID_HOME="$SDK"
mkdir -p target/apk/lib/arm64-v8a target/classes target/dex
cargo ndk -t arm64-v8a -P 26 -o target/apk/lib build
javac -source 8 -target 8 -classpath "$ANDROID_JAR" -d target/classes java/dev/gpui/a11ytest/MainActivity.java
"$TOOLS/d8" --lib "$ANDROID_JAR" --min-api 26 --output target/dex target/classes/dev/gpui/a11ytest/*.class
"$TOOLS/aapt" package -f -M AndroidManifest.xml -I "$ANDROID_JAR" -F target/test-unsigned.apk
cp target/dex/classes.dex target/apk/
(cd target/apk && zip -q -r ../test-unsigned.apk classes.dex lib)
"$TOOLS/zipalign" -f 4 target/test-unsigned.apk target/test.apk
"$TOOLS/apksigner" sign --ks "$HOME/.android/debug.keystore" --ks-pass pass:android target/test.apk
ADB="$SDK/platform-tools/adb"
"$ADB" -s "$SERIAL" install --no-incremental -r target/test.apk
"$ADB" -s "$SERIAL" logcat -c
"$ADB" -s "$SERIAL" shell am start -S -n dev.gpui.a11ytest/.MainActivity
sleep 2
"$ADB" -s "$SERIAL" logcat -d -b all > target/test.log
rg "GpuiA11yRegression|panicked|FATAL|Fatal signal" target/test.log || true
rg 'PASS API' target/test.log
