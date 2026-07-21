package com.ruvnet.tactical

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.WindowManager
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * Single-activity shell: start the embedded Rust server, then show its dashboard
 * in a full-screen WebView over loopback. Also runs the phone-native BLE
 * device-presence scanner that feeds the auxiliary overlay.
 */
class MainActivity : Activity() {

    private lateinit var web: WebView
    private var bleScanner: BleScanner? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Keep the tactical display awake — an operator is watching it live.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        // Start the server in LIVE mode (demo = false): it waits for the sensor
        // mesh to POST /api/reading. Flip to `true` to demo without hardware.
        TacticalServer.nativeStart(PORT, false)

        // BLE device-presence overlay (auxiliary — devices, not people).
        bleScanner = BleScanner(this, PORT)
        ensureBlePermissionAndScan()

        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            webViewClient = object : WebViewClient() {
                override fun onReceivedError(
                    view: WebView,
                    request: WebResourceRequest?,
                    error: WebResourceError?,
                ) {
                    // The native server may still be binding on cold start —
                    // retry the load shortly instead of showing an error page.
                    if (request?.isForMainFrame == true) {
                        view.postDelayed({ view.loadUrl(DASHBOARD_URL) }, 400)
                    }
                }
            }
        }
        setContentView(web)

        // Give the socket a moment to come up before the first load.
        web.postDelayed({ web.loadUrl(DASHBOARD_URL) }, 500)
    }

    override fun onDestroy() {
        bleScanner?.stop()
        web.destroy()
        super.onDestroy()
    }

    /**
     * Request the runtime scan permission (BLUETOOTH_SCAN on Android 12+, else
     * fine location on older releases) and start scanning once granted.
     */
    private fun ensureBlePermissionAndScan() {
        val needed = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            Manifest.permission.BLUETOOTH_SCAN
        } else {
            Manifest.permission.ACCESS_FINE_LOCATION
        }
        if (checkSelfPermission(needed) == PackageManager.PERMISSION_GRANTED) {
            bleScanner?.start()
        } else {
            requestPermissions(arrayOf(needed), REQ_BLE)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        // BLE is optional — if denied, the rest of the app runs fine without it.
        if (requestCode == REQ_BLE &&
            grantResults.isNotEmpty() &&
            grantResults[0] == PackageManager.PERMISSION_GRANTED
        ) {
            bleScanner?.start()
        }
    }

    companion object {
        private const val PORT = 8099
        private const val DASHBOARD_URL = "http://127.0.0.1:$PORT/"
        private const val REQ_BLE = 1001
    }
}
