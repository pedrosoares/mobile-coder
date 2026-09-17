#!/usr/bin/env bash
# Build, install and run the execution probe, then print the report.
#
# Works against a physical device (arm64-v8a) or an emulator (x86_64) - the APK
# carries both ABIs and the app picks the matching rootfs at runtime.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
pkg="net.pedrosoares.mobilecoder.execprobe"

# shellcheck source=/dev/null
[ -f "$root/env.sh" ] && . "$root/env.sh"

for tool in adb cargo-ndk; do
  command -v "$tool" >/dev/null || { echo "not on PATH: $tool (source ./env.sh?)" >&2; exit 1; }
done
[ -n "${ANDROID_NDK_HOME:-}" ] || { echo "ANDROID_NDK_HOME is not set" >&2; exit 1; }

if [ -z "$(adb devices | sed '1d' | grep -w device || true)" ]; then
  echo "no device on adb. Start one, or:  \$ANDROID_HOME/emulator/emulator -avd <name>" >&2
  exit 1
fi

# The probe is meaningless on a permissive device: the restriction it exists to
# measure simply is not applied.
enforce="$(adb shell getenforce 2>/dev/null | tr -d '\r')"
echo "==> SELinux: $enforce"
if [ "$enforce" != "Enforcing" ]; then
  echo "    WARNING: not Enforcing - results will not mean what they appear to." >&2
fi

if [ ! -f "$here/AndroidApp/app/src/main/assets/alpine-minirootfs-x86_64.tar" ]; then
  echo "==> assets missing; fetching"
  "$here/fetch-assets.sh"
fi

echo "==> building the Rust cdylib (arm64-v8a + x86_64)"
( cd "$root" && cargo ndk -t arm64-v8a -t x86_64 \
    -o "$here/AndroidApp/app/src/main/jniLibs" \
    build --release -p exec-probe )

echo "==> assembling the APK"
( cd "$here/AndroidApp" && ./gradlew --quiet assembleDebug )

apk="$here/AndroidApp/app/build/outputs/apk/debug/app-debug.apk"
echo "==> installing"
adb install -r -g "$apk" >/dev/null

# Start from a clean slate: a stale rootfs from a previous run would skip the
# extraction path and could mask a change.
adb shell pm clear "$pkg" >/dev/null 2>&1 || true
adb logcat -c
adb shell am start -n "$pkg/.MainActivity" >/dev/null

echo "==> waiting for the probe (extracting a rootfs takes a moment)"
for _ in $(seq 1 60); do
  sleep 2
  if adb logcat -d -s mc-exec-probe | grep -q -- "--- verdict ---"; then break; fi
done

echo
adb logcat -d -s mc-exec-probe | sed 's/^.*mc-exec-probe: //'
echo
echo "==> machine-readable report"
adb exec-out run-as "$pkg" cat files/probe-report.json 2>/dev/null \
  || echo "(could not pull probe-report.json; the log above is authoritative)"
