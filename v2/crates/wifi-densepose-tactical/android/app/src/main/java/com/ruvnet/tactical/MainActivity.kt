package com.ruvnet.tactical

import android.app.Activity
import android.os.Bundle
import android.view.WindowManager
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * Single-activity shell: start the embedded Rust server, then show its dashboard
 * in a full-screen WebView over loopback.
 */
class MainActivity : Activity() {

    private lateinit var web: WebView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Keep the tactical display awake — an operator is watching it live.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        // Start the server in LIVE mode (demo = false): it waits for the sensor
        // mesh to POST /api/reading. Flip to `true` to demo without hardware.
        TacticalServer.nativeStart(PORT, false)

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
        web.destroy()
        super.onDestroy()
    }

    companion object {
        private const val PORT = 8099
        private const val DASHBOARD_URL = "http://127.0.0.1:$PORT/"
    }
}
