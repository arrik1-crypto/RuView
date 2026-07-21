package com.ruvnet.tactical

/**
 * Thin JNI bridge to the embedded Rust tactical server.
 *
 * Loads `libwifi_densepose_tactical.so` (built by `../build-jni.sh`) and starts
 * the Axum server on a background thread bound to `0.0.0.0:<port>`, so the
 * ESP32/CSI sensor mesh on the LAN can POST readings to this phone.
 */
object TacticalServer {
    init {
        System.loadLibrary("wifi_densepose_tactical")
    }

    /**
     * Start the server (non-blocking).
     *
     * @param port TCP port to bind (<= 0 falls back to 8099).
     * @param demo when true, runs the built-in synthetic scenario instead of
     *             waiting for live readings.
     */
    external fun nativeStart(port: Int, demo: Boolean)
}
