#!/usr/bin/env bash
# Build, install and launch mobile-coder on a connected device or emulator.
#
#   usage: apps/android/run-app.sh [--logs]
#
# --logs tails the app's logcat afterwards, which is where the sandbox bootstrap
# reports rootfs installation and the first commands run inside the guest.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
pkg="net.pedrosoares.mobilecoder"

# shellcheck source=/dev/null
[ -f "$root/env.sh" ] && . "$root/env.sh"
export ANDROID_JAR="${ANDROID_JAR:-$ANDROID_HOME/platforms/android-36/android.jar}"

command -v adb >/dev/null || { echo "adb not on PATH (source ./env.sh?)" >&2; exit 1; }
if [ -z "$(adb devices | sed '1d' | grep -w device || true)" ]; then
  echo "no device on adb. Start one, e.g.:" >&2
  echo "  \$ANDROID_HOME/emulator/emulator -avd mc-probe -no-window -gpu host" >&2
  exit 1
fi

# proot and its shared libraries. Without these the sandbox cannot start, and it
# fails in a way that looks like the SELinux restriction rather than a missing file.
libs="$here/AndroidApp/app/src/main/jniLibs"
if [ ! -f "$libs/x86_64/libproot.so" ] || [ ! -f "$libs/arm64-v8a/libproot.so" ]; then
  echo "==> fetching proot and its libraries"
  "$root/tools/fetch-proot.sh" "$libs"
fi

# Build only for the ABI the target actually is - a full two-ABI Skia build is a
# long wait for a device that can use half of it.
abi="$(adb shell getprop ro.product.cpu.abi | tr -d '\r')"
echo "==> target abi: $abi"

echo "==> assembling (Gradle drives cargo-ndk)"
( cd "$here/AndroidApp" && ./gradlew --quiet assembleDebug -Pmc.abi="$abi" )

apk="$here/AndroidApp/app/build/outputs/apk/debug/app-debug.apk"
echo "==> installing $(du -h "$apk" | cut -f1)"
adb install -r "$apk" >/dev/null

adb logcat -c
adb shell am start -n "$pkg/.MainActivity" >/dev/null
echo "==> launched"

if [ "${1:-}" = "--logs" ]; then
  echo "==> logcat (ctrl-c to stop)"
  adb logcat -s mobile-coder RustStdoutStderr AndroidRuntime
fi
