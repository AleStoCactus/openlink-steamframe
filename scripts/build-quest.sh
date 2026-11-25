#!/usr/bin/env bash
# Builds the Quest app into a signed APK, and with --install also installs and starts it.
#
#   scripts/build-quest.sh [--debug] [--install]
#
# Needs: Rust target aarch64-linux-android, cargo-ndk, the Android NDK, SDK build-tools
# (aapt2, zipalign, apksigner), platform android-34 and a JDK. The OpenXR loader is downloaded
# from Maven Central on first use. Signs with ~/.android/debug.keystore (created if missing).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE=release
INSTALL=0
for a in "$@"; do
    case "$a" in
        --debug) PROFILE=debug ;;
        --install) INSTALL=1 ;;
        *) echo "unknown option $a" >&2; exit 2 ;;
    esac
done

: "${ANDROID_HOME:=/opt/android-sdk}"
: "${ANDROID_NDK_HOME:=${ANDROID_NDK:-/opt/android-ndk}}"
export ANDROID_NDK_HOME
PLATFORM_JAR="$ANDROID_HOME/platforms/android-34/android.jar"
BUILD_TOOLS="$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)"
tool() { if [ -x "$BUILD_TOOLS/$1" ]; then echo "$BUILD_TOOLS/$1"; else command -v "$1"; fi; }
AAPT2="$(tool aapt2)"; ZIPALIGN="$(tool zipalign)"; APKSIGNER="$(tool apksigner)"

LOADER_VERSION=1.1.65
CACHE="$ROOT/.cache/openxr-loader-$LOADER_VERSION"
LOADER="$CACHE/jni/arm64-v8a/libopenxr_loader.so"
if [ ! -f "$LOADER" ]; then
    echo "Downloading the Khronos OpenXR loader $LOADER_VERSION"
    mkdir -p "$CACHE"
    curl -sSfL -o "$CACHE/loader.aar" \
        "https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$LOADER_VERSION/openxr_loader_for_android-$LOADER_VERSION.aar"
    (cd "$CACHE" && unzip -oq loader.aar "jni/arm64-v8a/*")
fi

cd "$ROOT"
CARGO_FLAGS=()
[ "$PROFILE" = release ] && CARGO_FLAGS+=(--release)
cargo ndk -t arm64-v8a -P 32 build -p openlink-quest "${CARGO_FLAGS[@]}"

OUT="$ROOT/target/quest"
STAGE="$OUT/stage"
rm -rf "$STAGE"; mkdir -p "$STAGE/lib/arm64-v8a"
cp "target/aarch64-linux-android/$PROFILE/libopenlink_quest.so" "$STAGE/lib/arm64-v8a/"
cp "$LOADER" "$STAGE/lib/arm64-v8a/"

"$AAPT2" link -o "$OUT/unaligned.apk" --manifest crates/openlink-quest/android/AndroidManifest.xml -I "$PLATFORM_JAR"
python3 - "$OUT/unaligned.apk" "$STAGE" <<'PY'
import os, sys, zipfile
apk, stage = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(apk, "a", zipfile.ZIP_DEFLATED) as z:
    for d, _, files in os.walk(os.path.join(stage, "lib")):
        for f in files:
            path = os.path.join(d, f)
            z.write(path, os.path.relpath(path, stage))
PY
"$ZIPALIGN" -f -p 4 "$OUT/unaligned.apk" "$OUT/aligned.apk"

KEYSTORE="$HOME/.android/debug.keystore"
if [ ! -f "$KEYSTORE" ]; then
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android -alias androiddebugkey \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US" >/dev/null
fi
"$APKSIGNER" sign --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android --out "$OUT/openlink.apk" "$OUT/aligned.apk"
rm -f "$OUT/unaligned.apk" "$OUT/aligned.apk" "$OUT/openlink.apk.idsig"
echo "Built $OUT/openlink.apk"

if [ "$INSTALL" = 1 ]; then
    adb install -r "$OUT/openlink.apk"
    adb shell am start -n org.openlink.quest/android.app.NativeActivity
fi
