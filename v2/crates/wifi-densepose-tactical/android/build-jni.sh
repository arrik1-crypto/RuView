#!/usr/bin/env bash
# Cross-compile the tactical server into Android .so libraries and drop them
# where Gradle bundles them into the APK (app/src/main/jniLibs/<abi>/).
#
# Prerequisites (install on your build machine — NOT needed at runtime):
#   rustup target add aarch64-linux-android armv7-linux-androideabi \
#                     x86_64-linux-android i686-linux-android
#   cargo install cargo-ndk
#   Android NDK (Android Studio → SDK Manager → "NDK (Side by side)")
#   export ANDROID_NDK_HOME=/path/to/Android/Sdk/ndk/<version>
#
# Then just run:  ./android/build-jni.sh
set -euo pipefail

CRATE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
JNILIBS="$(cd "$(dirname "$0")" && pwd)/app/src/main/jniLibs"
mkdir -p "$JNILIBS"

cd "$CRATE_DIR"
# --lib builds only the cdylib (libwifi_densepose_tactical.so); the desktop
# binary target is skipped. --features android pulls the JNI entry point.
cargo ndk \
  -t arm64-v8a -t armeabi-v7a -t x86_64 -t x86 \
  -o "$JNILIBS" \
  build --release --lib --features android

echo "Built .so libs into: $JNILIBS"
ls -1 "$JNILIBS"
