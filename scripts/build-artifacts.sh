#!/usr/bin/env bash
# Build every release artifact in one go: the CLI binary, the desktop bundles
# (deb / rpm / AppImage) and the signed aarch64 APK.
#
# Only stops landrop-desktop processes that run from this repo's target/ dir, so
# a system-wide installed copy is left alone. The APK is signed with the local
# debug keystore (fine for sideloading) and also copied under a timestamped name
# so phones cannot mix it up with an older download.
set -euo pipefail

cd "$(dirname "$0")/.."

export JAVA_HOME="${JAVA_HOME:-/usr/lib/jvm/java-17-openjdk-amd64}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export NDK_HOME="${NDK_HOME:-$ANDROID_HOME/ndk/28.2.13676358}"

for PID in $(ps -eo pid,comm | awk '$2=="landrop-desktop"{print $1}'); do
  EXE=$(readlink -f "/proc/$PID/exe" 2>/dev/null || true)
  case "$EXE" in
    */landrop/target/*) kill "$PID" && echo "stopped repo build pid $PID ($EXE)";;
    *) echo "left running: pid $PID ($EXE)";;
  esac
done
sleep 1

echo "=== CLI binary ==="
make build
echo "=== desktop bundles ==="
make desktop-build
echo "=== phone APK (aarch64) ==="
make mobile-build-phone

BT=$(ls "$ANDROID_HOME/build-tools" | tail -1)
APK=desktop/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk
"$ANDROID_HOME/build-tools/$BT/zipalign" -p -f 4 "$APK" /tmp/final-aligned.apk
"$ANDROID_HOME/build-tools/$BT/apksigner" sign --ks "$HOME/.android/debug.keystore" \
  --ks-pass pass:android --key-pass pass:android --out bin/LanDrop-android-arm64.apk /tmp/final-aligned.apk

STAMP=$(date +%Y-%m-%d-%H%M)
cp bin/LanDrop-android-arm64.apk "bin/LanDrop-arm64-$STAMP.apk"
echo "apk: $(ls -la bin/LanDrop-android-arm64.apk)"
echo "dated copy: bin/LanDrop-arm64-$STAMP.apk"
echo "sha256: $(sha256sum bin/LanDrop-android-arm64.apk | cut -d' ' -f1)"
echo "deb: $(ls -la target/release/bundle/deb/*.deb)"
echo "DONE"
