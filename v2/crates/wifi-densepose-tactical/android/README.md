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
ESP32/CSI nodes ──(CSI→vitals DSP)──> ReadingInput JSON
       │  POST http://<phone-LAN-IP>:8099/api/reading
       ▼
  APK (0.0.0.0:8099) ── engine ── WebView @ 127.0.0.1:8099
```

The server binds `0.0.0.0`, so provision your sensor nodes with the **phone's
LAN IP** as the target (the RuView firmware's `--target-ip`). Each reading is the
distilled `ReadingInput` shape (`presence`, `breathing_bpm`, `movement`,
`occupancy`, `sensor_rssi`) — run the CSI→vitals DSP on the node/aggregator side
(MAT's `DetectionPipeline`), not on the phone. Post your floor plan once to
`/api/structure`; then stream `/api/reading` per room per cycle.

## Build

Prerequisites (on the build machine — Android Studio provides the SDK/NDK):

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi \
                  x86_64-linux-android i686-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=/path/to/Android/Sdk/ndk/<version>
```

1. **Cross-compile the native libs** (drops `.so` into `app/src/main/jniLibs/`):

   ```bash
   ./build-jni.sh
   ```

2. **Build the APK.** Open this `android/` folder in Android Studio and press
   Run, or from the CLI (after `gradle wrapper` or with a local Gradle 8.7+):

   ```bash
   ./gradlew assembleRelease     # unsigned: app/build/outputs/apk/release/
   ```

3. **Sign** for distribution (`apksigner` / an Android keystore) and install.

For a demo build with no hardware, set `nativeStart(PORT, true)` in
`MainActivity.kt` — the app then runs the built-in synthetic scenario.

## ⚠ Operational note

Decision-support only. Contacts are anonymous human presence — not identified as
hostage vs suspect, not confirmed armed. Absence never means a room is empty. See
the crate root docs for the full safety contract.
