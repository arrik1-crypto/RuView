# RuView Tactical — Android APK

A standalone APK that runs the tactical **console** on a phone/tablet: it embeds
the Rust engine + Axum server as a native library and shows the live floor-plan
dashboard in a WebView. Fully offline — the dashboard is compiled into the `.so`,
nothing is fetched at runtime.

## What this APK is (and is not)

- **It is** the operator's display + engine + aggregation point.
- **It is not** the sensor. Android exposes no WiFi CSI to apps — a phone cannot
  see through walls on its own. Person detection comes from an **external
  ESP32/CSI sensor mesh on the LAN**, which POSTs readings to this phone.
- No `ort` / ONNX is involved. This build depends on MAT with
  `default-features = false`; the ONNX runtime is never downloaded or bundled.

## Data flow

```
ESP32/CSI nodes ──> APK (0.0.0.0:8099) ── engine ── WebView @ 127.0.0.1:8099
```

The server binds `0.0.0.0`, so provision your sensor nodes with the **phone's
LAN IP** as the target (the RuView firmware's `--target-ip`). Post your floor
plan once to `POST /api/structure`, then feed one of two ingest endpoints per
room per cycle:

- **`POST /api/csi`** — raw CSI: `{ room_name|room_id, amplitudes[], phases[],
  sensor_rssi[] }`. The phone runs MAT's detection pipeline on-device (breathing
  / movement) and updates the picture once ~5 s of signal is buffered. Use this
  when nodes stream raw CSI.
- **`POST /api/reading`** — pre-distilled: `{ presence, breathing_bpm, movement,
  occupancy, sensor_rssi }`. Use this when an aggregator already ran the CSI→
  vitals DSP.

Either way, `sensor_rssi` (≥3 nodes) enables a triangulated point fix; fewer
gives room-level presence.

## Build

Prerequisites (on the build machine):

- **JDK 17**, **Android SDK** (platform 34 + build-tools + platform-tools), and
  the **Android NDK** (r26+). Android Studio installs all three via its SDK
  Manager; or use `sdkmanager` directly.
- Rust Android targets + `cargo-ndk`:

  ```bash
  rustup target add aarch64-linux-android armv7-linux-androideabi \
                    x86_64-linux-android i686-linux-android
  cargo install cargo-ndk
  export ANDROID_NDK_HOME=/path/to/Android/Sdk/ndk/<version>
  ```
- Tell Gradle where the SDK is — either `export ANDROID_HOME=/path/to/Android/Sdk`
  or create `android/local.properties` with `sdk.dir=/path/to/Android/Sdk`
  (machine-specific, not committed).

1. **Cross-compile the native libs FIRST** (drops `.so` into
   `app/src/main/jniLibs/`). This must happen before the Gradle build, or the
   app will hit `UnsatisfiedLinkError` at launch:

   ```bash
   ./build-jni.sh
   ```

2. **Build an installable (debug-signed) APK.** The Gradle wrapper is committed,
   so no Android Studio and no local Gradle install is needed:

   ```bash
   ./gradlew assembleDebug       # app/build/outputs/apk/debug/app-debug.apk
   ```

   `assembleDebug` auto-signs with the debug keystore, so it installs directly.
   `assembleRelease` produces an **unsigned** APK — for distribution, add a
   `signingConfig` + keystore (or sign with `apksigner`) before installing.

3. **Install it** on a device with USB debugging on:

   ```bash
   adb install -r app/build/outputs/apk/debug/app-debug.apk
   ```

   (Or open this `android/` folder in Android Studio and press Run.)

For a first-run smoke test with no hardware, set `nativeStart(PORT, true)` in
`MainActivity.kt` — the app then runs the built-in synthetic scenario and you
should see contacts + the sensor panel populate immediately.

## Bluetooth overlay (auxiliary)

The app also scans with the phone's own BLE radio and shows detected devices in a
separate "Bluetooth — devices, not people" panel (feeds `POST /api/ble`). It asks
for `BLUETOOTH_SCAN` at first launch (declared `neverForLocation`, so no location
permission); if denied, everything else still works. **This detects transmitting
Bluetooth devices by distance only — it is not a person-finder.** A person without
a discoverable device is invisible to it, and it is never drawn on the floor plan.

## ⚠ Operational note

Decision-support only. Contacts are anonymous human presence — not identified as
hostage vs suspect, not confirmed armed. Absence never means a room is empty. See
the crate root docs for the full safety contract.
